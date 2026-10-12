// Single-database search backend for development and small deployments.
// Lexical intent search over an inverted index, plus the same hunk and LSH
// band lookups a production key-value backend performs.

import { similarity, terms } from "../core/text";
import type { HunkHit, IntentDoc, IntentHit, PatchDoc, SearchIndex, SketchHit } from "./types";

const placeholders = (n: number) => Array.from({ length: n }, () => "?").join(",");

export class D1SearchIndex implements SearchIndex {
  constructor(private db: D1Database) {}

  async indexIntent(doc: IntentDoc) {
    const ts = terms(doc.text);
    if (ts.length === 0) return;
    await this.db.batch(
      ts.map((t) =>
        this.db.prepare("INSERT OR IGNORE INTO intent_terms (term, intent_id, project_id) VALUES (?, ?, ?)").bind(t, doc.id, doc.projectId),
      ),
    );
  }

  async indexPatch(doc: PatchDoc) {
    await this.db.batch([
      ...doc.hunks.map((h) =>
        this.db
          .prepare("INSERT OR IGNORE INTO patch_hunks (project_id, path, fingerprint, patch_id) VALUES (?, ?, ?, ?)")
          .bind(doc.projectId, h.path, h.fingerprint, doc.id),
      ),
      ...(doc.sketch
        ? [
            this.db
              .prepare("INSERT OR REPLACE INTO patch_sketches (patch_id, project_id, signature) VALUES (?, ?, ?)")
              .bind(doc.id, doc.projectId, doc.sketch.signature),
            ...doc.sketch.bands.map((b) =>
              this.db.prepare("INSERT OR IGNORE INTO patch_bands (project_id, band, patch_id) VALUES (?, ?, ?)").bind(doc.projectId, b, doc.id),
            ),
          ]
        : []),
    ]);
  }

  async searchIntents(projectId: string, text: string, k: number): Promise<IntentHit[]> {
    const q = terms(text);
    if (q.length === 0) return [];
    const { results: cands } = await this.db
      .prepare(
        `SELECT i.id, i.text FROM intents i
         JOIN (SELECT intent_id, COUNT(*) AS hits FROM intent_terms
               WHERE project_id = ? AND term IN (${placeholders(q.length)}) GROUP BY intent_id
               ORDER BY hits DESC LIMIT 200) t ON t.intent_id = i.id`,
      )
      .bind(projectId, ...q)
      .all<{ id: string; text: string }>();

    const vocab = [...new Set([...q, ...cands.flatMap((c) => terms(c.text))])];
    const total =
      (await this.db.prepare("SELECT COUNT(*) AS n FROM intents WHERE project_id = ?").bind(projectId).first<{ n: number }>())?.n ?? 0;
    const { results: dfs } = await this.db
      .prepare(`SELECT term, COUNT(*) AS df FROM intent_terms WHERE project_id = ? AND term IN (${placeholders(vocab.length)}) GROUP BY term`)
      .bind(projectId, ...vocab)
      .all<{ term: string; df: number }>();
    const df = new Map(dfs.map((r) => [r.term, r.df]));
    const idf = (t: string) => Math.log(1 + (total + 1) / ((df.get(t) ?? 0) + 1));

    return cands
      .map((c) => ({ id: c.id, relevance: similarity(q, terms(c.text), idf) }))
      .sort((a, b) => b.relevance - a.relevance)
      .slice(0, k);
  }

  async patchesByHunks(projectId: string, fingerprints: string[], k: number): Promise<HunkHit[]> {
    if (fingerprints.length === 0) return [];
    const { results } = await this.db
      .prepare(
        `SELECT h.patch_id, h.shared, (SELECT COUNT(*) FROM patch_hunks x WHERE x.patch_id = h.patch_id) AS total
         FROM (SELECT patch_id, COUNT(*) AS shared FROM patch_hunks
               WHERE project_id = ? AND fingerprint IN (${placeholders(fingerprints.length)})
               GROUP BY patch_id ORDER BY shared DESC LIMIT ?) h`,
      )
      .bind(projectId, ...fingerprints, k)
      .all<{ patch_id: string; shared: number; total: number }>();
    return results.map((r) => ({ patchId: r.patch_id, shared: r.shared, total: r.total }));
  }

  async patchesByBands(projectId: string, bands: string[], k: number): Promise<SketchHit[]> {
    if (bands.length === 0) return [];
    const { results } = await this.db
      .prepare(
        `SELECT s.patch_id, s.signature FROM patch_sketches s
         JOIN (SELECT patch_id, COUNT(*) AS hits FROM patch_bands
               WHERE project_id = ? AND band IN (${placeholders(bands.length)})
               GROUP BY patch_id ORDER BY hits DESC LIMIT ?) b ON b.patch_id = s.patch_id`,
      )
      .bind(projectId, ...bands, k)
      .all<{ patch_id: string; signature: string }>();
    return results.map((r) => ({ patchId: r.patch_id, signature: r.signature }));
  }
}
