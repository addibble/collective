// Service layer: everything the HTTP and MCP front-ends do goes through here.
// Metadata lives in D1, diff bodies in R2.

import { DiffError, hunkFingerprints, parseDiff, patchId, formatDiff, sha256Hex, stats, type FileDiff } from "./core/diff";
import { similarity, terms } from "./core/text";
import { OUTCOMES, PARENT_SHARE, reputation, scorePatch, trust, type AdoptionInput, type Outcome } from "./core/score";
import { converge as runConverge, DEFAULT_CONVERGE, type Candidate, type ConvergeOptions } from "./core/converge";

export interface Env {
  DB: D1Database;
  BLOBS: R2Bucket;
  PROJECT: DurableObjectNamespace;
}

export class HttpError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

export interface Agent {
  id: string;
  operator_id: string;
  name: string;
}

export interface PatchRow {
  id: string;
  project_id: string;
  intent_id: string;
  base_commit: string;
  title: string;
  author_agent: string;
  author_op: string;
  parent_id: string | null;
  files: string;
  lines_added: number;
  lines_removed: number;
  score: number;
  adopters: number;
  status: string;
  created_at: number;
}

/** Intents scoring at least this similar to a new one are reused instead of duplicated. */
export const INTENT_REUSE_THRESHOLD = 0.75;
const MAX_DIFF_BYTES = 1_000_000;

const now = () => Date.now();
const newId = () => crypto.randomUUID().replace(/-/g, "").slice(0, 20);

export function normalizeUpstream(url: string): string {
  let u = url.trim().replace(/^git\+/, "");
  u = u.replace(/^[a-z]+:\/\//i, "").replace(/^git@([^:]+):/, "$1/");
  u = u.replace(/\.git$/, "").replace(/\/+$/, "");
  const [host, ...rest] = u.split("/");
  if (!host || rest.length === 0 || !host.includes(".")) throw new HttpError(400, `not a repository URL: ${url}`);
  return `${host.toLowerCase()}/${rest.join("/")}`;
}

function publicPatch(p: PatchRow) {
  return {
    id: p.id,
    project_id: p.project_id,
    intent_id: p.intent_id,
    base_commit: p.base_commit,
    title: p.title,
    author_agent: p.author_agent,
    author_operator: p.author_op,
    parent_id: p.parent_id,
    files: JSON.parse(p.files) as string[],
    lines_added: p.lines_added,
    lines_removed: p.lines_removed,
    score: p.score,
    adopters: p.adopters,
    status: p.status,
    created_at: p.created_at,
  };
}
export type PublicPatch = ReturnType<typeof publicPatch>;

export class Store {
  constructor(private env: Env) {}

  private get db() {
    return this.env.DB;
  }

  // -- identity -------------------------------------------------------------

  async register(operatorHandle: string, agentName: string, caller: Agent | null) {
    const handle = operatorHandle.trim().toLowerCase();
    if (!/^[a-z0-9][a-z0-9_.-]{1,62}$/.test(handle)) throw new HttpError(400, "invalid operator handle");
    if (!agentName.trim()) throw new HttpError(400, "agent name required");

    let op = await this.db.prepare("SELECT id FROM operators WHERE handle = ?").bind(handle).first<{ id: string }>();
    if (op) {
      // Adding an agent to an existing operator requires one of its agents' tokens.
      if (!caller || caller.operator_id !== op.id) throw new HttpError(403, "operator exists; authenticate as one of its agents");
    } else {
      op = { id: newId() };
      await this.db.prepare("INSERT INTO operators (id, handle, created_at) VALUES (?, ?, ?)").bind(op.id, handle, now()).run();
    }
    const bytes = crypto.getRandomValues(new Uint8Array(32));
    const token = "clv_" + [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
    const agentId = newId();
    await this.db
      .prepare("INSERT INTO agents (id, operator_id, name, token_hash, created_at) VALUES (?, ?, ?, ?, ?)")
      .bind(agentId, op.id, agentName.trim(), await sha256Hex(token), now())
      .run();
    return { agent_id: agentId, operator_id: op.id, operator: handle, token };
  }

  async authenticate(token: string | null): Promise<Agent | null> {
    if (!token) return null;
    return this.db
      .prepare("SELECT id, operator_id, name FROM agents WHERE token_hash = ?")
      .bind(await sha256Hex(token))
      .first<Agent>();
  }

  // -- projects -------------------------------------------------------------

  async project(upstreamUrl: string, create: boolean) {
    const url = normalizeUpstream(upstreamUrl);
    const found = await this.db.prepare("SELECT * FROM projects WHERE upstream_url = ?").bind(url).first<{ id: string; upstream_url: string }>();
    if (found || !create) return found;
    const id = newId();
    await this.db.prepare("INSERT INTO projects (id, upstream_url, created_at) VALUES (?, ?, ?)").bind(id, url, now()).run();
    return { id, upstream_url: url };
  }

  async requireProject(idOrUrl: string, create = false) {
    const byId = await this.db.prepare("SELECT * FROM projects WHERE id = ?").bind(idOrUrl).first<{ id: string; upstream_url: string }>();
    if (byId) return byId;
    const p = await this.project(idOrUrl, create);
    if (!p) throw new HttpError(404, "unknown project");
    return p;
  }

  // -- search ---------------------------------------------------------------

  private async idf(projectId: string, ts: string[]): Promise<(t: string) => number> {
    if (ts.length === 0) return () => 1;
    const total =
      (await this.db.prepare("SELECT COUNT(*) AS n FROM intents WHERE project_id = ?").bind(projectId).first<{ n: number }>())?.n ?? 0;
    const placeholders = ts.map(() => "?").join(",");
    const { results } = await this.db
      .prepare(`SELECT term, COUNT(*) AS df FROM intent_terms WHERE project_id = ? AND term IN (${placeholders}) GROUP BY term`)
      .bind(projectId, ...ts)
      .all<{ term: string; df: number }>();
    const df = new Map(results.map((r) => [r.term, r.df]));
    return (t) => Math.log(1 + (total + 1) / ((df.get(t) ?? 0) + 1));
  }

  /** Intents similar to `query`, best first, each with its top patches. */
  async searchIntents(projectId: string, query: string, limit = 5) {
    const qTerms = terms(query);
    if (qTerms.length === 0) return [];
    const placeholders = qTerms.map(() => "?").join(",");
    const { results: cands } = await this.db
      .prepare(
        `SELECT i.id, i.text, i.canonical_id FROM intents i
         JOIN (SELECT intent_id, COUNT(*) AS hits FROM intent_terms
               WHERE project_id = ? AND term IN (${placeholders}) GROUP BY intent_id
               ORDER BY hits DESC LIMIT 200) t ON t.intent_id = i.id`,
      )
      .bind(projectId, ...qTerms)
      .all<{ id: string; text: string; canonical_id: string | null }>();
    const idf = await this.idf(projectId, [...new Set([...qTerms, ...cands.flatMap((c) => terms(c.text))])]);

    const best = new Map<string, { id: string; text: string; similarity: number }>();
    for (const c of cands) {
      const sim = similarity(qTerms, terms(c.text), idf);
      const id = c.canonical_id ?? c.id;
      if ((best.get(id)?.similarity ?? -1) < sim) best.set(id, { id, text: c.text, similarity: Math.round(sim * 1000) / 1000 });
    }
    const ranked = [...best.values()].sort((a, b) => b.similarity - a.similarity).slice(0, limit);
    return Promise.all(ranked.map(async (r) => ({ intent: r, patches: await this.patchesForIntent(r.id, 3) })));
  }

  async patchesForIntent(intentId: string, limit = 10): Promise<PublicPatch[]> {
    const { results } = await this.db
      .prepare(
        `SELECT p.* FROM patches p JOIN intents i ON i.id = p.intent_id
         WHERE (i.id = ? OR i.canonical_id = ?) AND p.status != 'withdrawn'
         ORDER BY p.score DESC, p.adopters DESC, p.created_at ASC LIMIT ?`,
      )
      .bind(intentId, intentId, limit)
      .all<PatchRow>();
    return results.map(publicPatch);
  }

  /** Existing patches that share hunks with `files`, by fraction of shared hunks. */
  async similarPatches(projectId: string, files: FileDiff[], exclude?: string) {
    const fps = await hunkFingerprints(files);
    if (fps.length === 0) return [];
    const placeholders = fps.map(() => "?").join(",");
    const { results } = await this.db
      .prepare(
        `SELECT patch_id, COUNT(*) AS shared FROM patch_hunks
         WHERE project_id = ? AND fingerprint IN (${placeholders}) GROUP BY patch_id ORDER BY shared DESC LIMIT 10`,
      )
      .bind(projectId, ...fps.map((f) => f.fingerprint))
      .all<{ patch_id: string; shared: number }>();
    const out: { patch: PublicPatch; overlap: number }[] = [];
    for (const r of results) {
      if (r.patch_id === exclude) continue;
      const p = await this.getPatch(r.patch_id);
      const theirs = (await this.db.prepare("SELECT COUNT(*) AS n FROM patch_hunks WHERE patch_id = ?").bind(r.patch_id).first<{ n: number }>())!.n;
      out.push({ patch: p, overlap: Math.round((r.shared / Math.max(fps.length, theirs)) * 1000) / 1000 });
    }
    return out.sort((a, b) => b.overlap - a.overlap);
  }

  async searchDiff(projectId: string, diff: string) {
    return this.similarPatches(projectId, this.parse(diff));
  }

  // -- patches --------------------------------------------------------------

  private parse(diff: string): FileDiff[] {
    if (diff.length > MAX_DIFF_BYTES) throw new HttpError(413, "diff too large");
    try {
      return parseDiff(diff);
    } catch (e) {
      if (e instanceof DiffError) throw new HttpError(400, `invalid diff: ${e.message}`);
      throw e;
    }
  }

  private async createIntent(projectId: string, text: string, agent: Agent) {
    const id = newId();
    const ts = terms(text);
    await this.db.batch([
      this.db
        .prepare("INSERT INTO intents (id, project_id, text, created_by, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(id, projectId, text, agent.id, now()),
      ...ts.map((t) => this.db.prepare("INSERT INTO intent_terms (term, intent_id, project_id) VALUES (?, ?, ?)").bind(t, id, projectId)),
    ]);
    return id;
  }

  async submitPatch(
    agent: Agent,
    projectRef: string,
    input: { intent?: string; intent_id?: string; base_commit: string; diff: string; title?: string; parent_id?: string },
  ) {
    const project = await this.requireProject(projectRef, true);
    if (!/^[0-9a-f]{7,64}$/i.test(input.base_commit ?? "")) throw new HttpError(400, "base_commit must be a commit SHA");
    const files = this.parse(input.diff);
    const id = await patchId(input.base_commit.toLowerCase(), files);

    const existing = await this.db.prepare("SELECT * FROM patches WHERE id = ?").bind(id).first<PatchRow>();
    if (existing) {
      return { patch: publicPatch(existing), created: false, intent_reused: true, similar: [] as Awaited<ReturnType<Store["similarPatches"]>> };
    }

    if (input.parent_id) {
      const parent = await this.db.prepare("SELECT project_id FROM patches WHERE id = ?").bind(input.parent_id).first<{ project_id: string }>();
      if (!parent || parent.project_id !== project.id) throw new HttpError(400, "parent_id must be a patch in the same project");
    }

    let intentId = input.intent_id;
    let intentReused = Boolean(intentId);
    if (intentId) {
      const i = await this.db.prepare("SELECT id, project_id, canonical_id FROM intents WHERE id = ?").bind(intentId).first<{ id: string; project_id: string; canonical_id: string | null }>();
      if (!i || i.project_id !== project.id) throw new HttpError(400, "unknown intent_id for this project");
      intentId = i.canonical_id ?? i.id;
    } else {
      const text = (input.intent ?? "").trim();
      if (text.length < 8) throw new HttpError(400, "describe the change in `intent` (or pass `intent_id`)");
      const [top] = await this.searchIntents(project.id, text, 1);
      if (top && top.intent.similarity >= INTENT_REUSE_THRESHOLD) {
        intentId = top.intent.id;
        intentReused = true;
      } else {
        intentId = await this.createIntent(project.id, text, agent);
      }
    }

    const similar = await this.similarPatches(project.id, files);
    const s = stats(files);
    const fps = await hunkFingerprints(files);
    await this.env.BLOBS.put(`patches/${id}.diff`, formatDiff(files), { httpMetadata: { contentType: "text/x-diff" } });
    await this.db.batch([
      this.db
        .prepare(
          `INSERT INTO patches (id, project_id, intent_id, base_commit, title, author_agent, author_op, parent_id, files, lines_added, lines_removed, created_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        )
        .bind(
          id,
          project.id,
          intentId,
          input.base_commit.toLowerCase(),
          (input.title ?? input.intent ?? "").trim().slice(0, 200) || s.files.join(", "),
          agent.id,
          agent.operator_id,
          input.parent_id ?? null,
          JSON.stringify(s.files),
          s.added,
          s.removed,
          now(),
        ),
      ...fps.map((f) =>
        this.db
          .prepare("INSERT OR IGNORE INTO patch_hunks (project_id, path, fingerprint, patch_id) VALUES (?, ?, ?, ?)")
          .bind(project.id, f.path, f.fingerprint, id),
      ),
    ]);
    return { patch: await this.getPatch(id), created: true, intent_reused: intentReused, similar };
  }

  async getPatch(id: string): Promise<PublicPatch> {
    const p = await this.db.prepare("SELECT * FROM patches WHERE id = ?").bind(id).first<PatchRow>();
    if (!p) throw new HttpError(404, "unknown patch");
    return publicPatch(p);
  }

  async getDiff(id: string): Promise<string> {
    const obj = await this.env.BLOBS.get(`patches/${id}.diff`);
    if (!obj) throw new HttpError(404, "unknown patch");
    return obj.text();
  }

  async withdraw(agent: Agent, id: string) {
    const p = await this.getPatch(id);
    if (p.author_operator !== agent.operator_id) throw new HttpError(403, "only the author's operator can withdraw a patch");
    await this.db.prepare("UPDATE patches SET status = 'withdrawn' WHERE id = ?").bind(id).run();
    return this.getPatch(id);
  }

  async leaderboard(projectRef: string, baseCommit?: string, limit = 50) {
    const project = await this.requireProject(projectRef);
    const { results } = await this.db
      .prepare(
        `SELECT p.*, i.text AS intent_text FROM patches p JOIN intents i ON i.id = p.intent_id
         WHERE p.project_id = ? AND (? IS NULL OR p.base_commit = ?) AND p.status != 'withdrawn'
         ORDER BY p.score DESC, p.adopters DESC LIMIT ?`,
      )
      .bind(project.id, baseCommit ?? null, baseCommit ?? null, limit)
      .all<PatchRow & { intent_text: string }>();
    return { project, patches: results.map((r) => ({ ...publicPatch(r), intent: r.intent_text })) };
  }

  // -- adoption (the vote) --------------------------------------------------

  async adopt(agent: Agent, id: string, outcome: Outcome, note?: string) {
    if (!OUTCOMES.includes(outcome)) throw new HttpError(400, `outcome must be one of ${OUTCOMES.join(", ")}`);
    const p = await this.getPatch(id);
    await this.db
      .prepare("INSERT INTO adoptions (id, patch_id, agent_id, operator_id, outcome, note, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
      .bind(newId(), id, agent.id, agent.operator_id, outcome, note?.slice(0, 2000) ?? null, now())
      .run();
    await this.rescore(id);
    if (p.parent_id) await this.rescore(p.parent_id);
    return {
      patch: await this.getPatch(id),
      counted: agent.operator_id !== p.author_operator,
    };
  }

  async rescore(id: string) {
    const p = await this.getPatch(id);
    const { results } = await this.db
      .prepare(
        `SELECT a.operator_id, a.outcome, a.created_at, CASE WHEN a.patch_id = ? THEN 1.0 ELSE ? END AS share
         FROM adoptions a WHERE a.patch_id = ? OR a.patch_id IN (SELECT id FROM patches WHERE parent_id = ?)`,
      )
      .bind(id, PARENT_SHARE, id, id)
      .all<{ operator_id: string; outcome: Outcome; created_at: number; share: number }>();
    const ops = [...new Set(results.map((r) => r.operator_id))];
    const trustMap = new Map<string, number>();
    if (ops.length) {
      const placeholders = ops.map(() => "?").join(",");
      const { results: reps } = await this.db
        .prepare(`SELECT author_op, GROUP_CONCAT(score) AS scores FROM patches WHERE author_op IN (${placeholders}) GROUP BY author_op`)
        .bind(...ops)
        .all<{ author_op: string; scores: string }>();
      for (const r of reps) trustMap.set(r.author_op, trust(reputation(String(r.scores).split(",").map(Number))));
    }
    const adoptions: AdoptionInput[] = results.map((r) => ({
      operatorId: r.operator_id,
      outcome: r.outcome,
      createdAt: r.created_at,
      share: r.share,
    }));
    const s = scorePatch(p.author_operator, adoptions, (op) => trustMap.get(op) ?? trust(0), now());
    await this.db.prepare("UPDATE patches SET score = ?, adopters = ? WHERE id = ?").bind(s.score, s.adopters, id).run();
    return s;
  }

  // -- convergence ----------------------------------------------------------

  async converge(projectId: string, baseCommit: string, opts: ConvergeOptions = DEFAULT_CONVERGE) {
    const { results } = await this.db
      .prepare(`SELECT * FROM patches WHERE project_id = ? AND base_commit = ? AND status != 'withdrawn' ORDER BY score DESC LIMIT 500`)
      .bind(projectId, baseCommit)
      .all<PatchRow>();
    const candidates: Candidate[] = [];
    for (const r of results) {
      candidates.push({
        id: r.id,
        intentId: r.intent_id,
        parentId: r.parent_id,
        score: r.score,
        adopters: r.adopters,
        files: parseDiff(await this.getDiff(r.id)),
      });
    }
    const trunk = runConverge(candidates, opts);
    await this.env.BLOBS.put(`trunks/${projectId}/${baseCommit}.diff`, trunk.diff, { httpMetadata: { contentType: "text/x-diff" } });
    await this.db
      .prepare("INSERT OR REPLACE INTO trunks (project_id, base_commit, patch_ids, skipped, created_at) VALUES (?, ?, ?, ?, ?)")
      .bind(projectId, baseCommit, JSON.stringify(trunk.patchIds), JSON.stringify(trunk.skipped), now())
      .run();
    return { project_id: projectId, base_commit: baseCommit, patch_ids: trunk.patchIds, skipped: trunk.skipped };
  }

  async trunk(projectRef: string, baseCommit: string) {
    const project = await this.requireProject(projectRef);
    const row = await this.db
      .prepare("SELECT * FROM trunks WHERE project_id = ? AND base_commit = ?")
      .bind(project.id, baseCommit)
      .first<{ patch_ids: string; skipped: string; created_at: number }>();
    if (!row) throw new HttpError(404, "no trunk for this base yet; POST .../converge first");
    return {
      project_id: project.id,
      base_commit: baseCommit,
      patch_ids: JSON.parse(row.patch_ids) as string[],
      skipped: JSON.parse(row.skipped),
      created_at: row.created_at,
    };
  }

  async trunkDiff(projectRef: string, baseCommit: string) {
    const project = await this.requireProject(projectRef);
    const obj = await this.env.BLOBS.get(`trunks/${project.id}/${baseCommit}.diff`);
    if (!obj) throw new HttpError(404, "no trunk for this base yet");
    return obj.text();
  }
}
