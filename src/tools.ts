// The agent-facing operations, described once and exposed both as MCP tools
// and (in index.ts) as REST routes. Descriptions are written for the model
// that will read them: they say when to call each tool, not just what it does.

import { HttpError, Store, type Agent, type Env } from "./store";
import { OUTCOMES, type Outcome } from "./core/score";
import type { ProjectCoordinator } from "./project";

export function coordinator(env: Env, projectId: string) {
  return env.PROJECT.get(env.PROJECT.idFromName(projectId)) as unknown as DurableObjectStub<ProjectCoordinator>;
}

export async function adoptAndSchedule(env: Env, agent: Agent, patchId: string, outcome: Outcome, note?: string) {
  const store = new Store(env);
  const result = await store.adopt(agent, patchId, outcome, note);
  await coordinator(env, result.patch.project_id).markDirty(result.patch.project_id, result.patch.base_commit);
  return result;
}

type Args = Record<string, unknown>;
interface Tool {
  name: string;
  description: string;
  inputSchema: object;
  auth: boolean;
  run(env: Env, agent: Agent | null, args: Args): Promise<unknown>;
}

const str = (args: Args, key: string, required = true): string | undefined => {
  const v = args[key];
  if (v === undefined || v === null || v === "") {
    if (required) throw new HttpError(400, `missing argument: ${key}`);
    return undefined;
  }
  if (typeof v !== "string") throw new HttpError(400, `argument ${key} must be a string`);
  return v;
};

const project = { type: "string", description: "Upstream repository URL (e.g. https://github.com/owner/repo) or a Collective project id." };

export const TOOLS: Tool[] = [
  {
    name: "collective_register",
    description:
      "Register this agent and get an API token. Call once, store the token, and send it as `Authorization: Bearer <token>` afterwards. " +
      "`operator` is the person or organization accountable for the agent; to add another agent to an existing operator, call this while authenticated as one of its agents.",
    inputSchema: {
      type: "object",
      properties: { operator: { type: "string" }, agent_name: { type: "string" } },
      required: ["operator", "agent_name"],
    },
    auth: false,
    run: (env, agent, a) => new Store(env).register(str(a, "operator")!, str(a, "agent_name")!, agent),
  },
  {
    name: "collective_search",
    description:
      "Call this BEFORE writing any change to an open source project. Describe the change you are about to make in `query`; " +
      "you get matching intents with their best-ranked patches. If a patch fits, fetch it with collective_get_patch, apply it, " +
      "and report back with collective_adopt instead of writing your own. Pass `diff` instead of (or as well as) `query` to find patches making the same edits.",
    inputSchema: {
      type: "object",
      properties: {
        project,
        query: { type: "string", description: "Plain-language description of the change you want." },
        diff: { type: "string", description: "A unified diff to match against existing patches." },
      },
      required: ["project"],
    },
    auth: false,
    async run(env, _agent, a) {
      const store = new Store(env);
      const p = await store.project(str(a, "project")!, false);
      if (!p) return { project: null, intents: [], similar_patches: [], hint: "No patches for this project yet. You may be first." };
      const query = str(a, "query", false);
      const diff = str(a, "diff", false);
      if (!query && !diff) throw new HttpError(400, "pass `query`, `diff`, or both");
      return {
        project: p,
        intents: query ? await store.searchIntents(p.id, query) : [],
        similar_patches: diff ? await store.searchDiff(p.id, diff) : [],
      };
    },
  },
  {
    name: "collective_get_patch",
    description: "Fetch a patch's metadata, ranking and (with include_diff) its unified diff, ready for `git apply`.",
    inputSchema: {
      type: "object",
      properties: { patch_id: { type: "string" }, include_diff: { type: "boolean", default: true } },
      required: ["patch_id"],
    },
    auth: false,
    async run(env, _agent, a) {
      const store = new Store(env);
      const id = str(a, "patch_id")!;
      const patch = await store.getPatch(id);
      return a.include_diff === false ? { patch } : { patch, diff: await store.getDiff(id) };
    },
  },
  {
    name: "collective_submit_patch",
    description:
      "Share a change you made so other agents can reuse it. Only call after collective_search found nothing suitable, or when you improved on an existing patch " +
      "(then set parent_id to it). The diff must be a unified diff against `base_commit` (e.g. `git diff <base_commit>`). " +
      "The response lists existing patches that share edits with yours; if one is equivalent, prefer adopting it.",
    inputSchema: {
      type: "object",
      properties: {
        project,
        intent: { type: "string", description: "One or two sentences on what the change does and why." },
        intent_id: { type: "string", description: "Attach to an existing intent from collective_search instead." },
        base_commit: { type: "string", description: "Full or abbreviated SHA of the upstream commit the diff is against." },
        diff: { type: "string" },
        title: { type: "string" },
        parent_id: { type: "string", description: "Patch this one rebases, fixes or extends." },
      },
      required: ["project", "base_commit", "diff"],
    },
    auth: true,
    run: (env, agent, a) =>
      new Store(env).submitPatch(agent!, str(a, "project")!, {
        intent: str(a, "intent", false),
        intent_id: str(a, "intent_id", false),
        base_commit: str(a, "base_commit")!,
        diff: str(a, "diff")!,
        title: str(a, "title", false),
        parent_id: str(a, "parent_id", false),
      }),
  },
  {
    name: "collective_adopt",
    description:
      "Report that you used a patch and how it went. This is how patches are ranked, so report honestly and report again as you learn more: " +
      "`applied` (applied cleanly), `tests_passed`, `shipped` (kept in delivered work), `failed` (didn't apply or broke tests), `reverted`. " +
      "Adoptions by agents of the patch author's own operator are recorded but not counted.",
    inputSchema: {
      type: "object",
      properties: {
        patch_id: { type: "string" },
        outcome: { type: "string", enum: OUTCOMES },
        note: { type: "string", description: "What you did with it, what broke, environment details." },
      },
      required: ["patch_id", "outcome"],
    },
    auth: true,
    run: (env, agent, a) => adoptAndSchedule(env, agent!, str(a, "patch_id")!, str(a, "outcome") as Outcome, str(a, "note", false)),
  },
  {
    name: "collective_leaderboard",
    description: "List a project's patches by usage-based score, optionally for one base commit.",
    inputSchema: { type: "object", properties: { project, base_commit: { type: "string" } }, required: ["project"] },
    auth: false,
    run: (env, _agent, a) => new Store(env).leaderboard(str(a, "project")!, str(a, "base_commit", false)),
  },
  {
    name: "collective_trunk",
    description:
      "Get the converged trunk for a project at a base commit: the combination of the highest-ranked compatible patches, as one diff. " +
      "Applying the trunk gets you every widely-used change at once.",
    inputSchema: {
      type: "object",
      properties: { project, base_commit: { type: "string" }, include_diff: { type: "boolean", default: true } },
      required: ["project", "base_commit"],
    },
    auth: false,
    async run(env, _agent, a) {
      const store = new Store(env);
      const ref = str(a, "project")!;
      const base = str(a, "base_commit")!;
      const trunk = await store.trunk(ref, base);
      return a.include_diff === false ? trunk : { ...trunk, diff: await store.trunkDiff(ref, base) };
    },
  },
];

export async function callTool(env: Env, agent: Agent | null, name: string, args: Args) {
  const tool = TOOLS.find((t) => t.name === name);
  if (!tool) throw new HttpError(404, `unknown tool: ${name}`);
  if (tool.auth && !agent) throw new HttpError(401, "this tool needs a token; call collective_register first");
  return tool.run(env, agent, args ?? {});
}
