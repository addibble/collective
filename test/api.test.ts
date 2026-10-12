import { describe, expect, it } from "vitest";
import app from "../src/index";
import { env as makeEnv } from "./shims";
import { applies, makeDiff, repo } from "./git";

const UPSTREAM = "https://github.com/example/widget.git";

function client() {
  const env = makeEnv();
  const call = async (method: string, path: string, body?: unknown, token?: string) => {
    const res = await app.request(
      path,
      {
        method,
        headers: { "content-type": "application/json", ...(token ? { authorization: `Bearer ${token}` } : {}) },
        body: body === undefined ? undefined : JSON.stringify(body),
      },
      env,
    );
    const ct = res.headers.get("content-type") ?? "";
    return { status: res.status, json: ct.includes("json") ? await res.json() : await res.text() } as { status: number; json: any };
  };
  const mcp = async (method: string, params: unknown, token?: string) =>
    (await call("POST", "/mcp", { jsonrpc: "2.0", id: 1, method, params }, token)).json;
  return { env, call, mcp };
}

async function agentFor(c: ReturnType<typeof client>, operator: string) {
  const r = await c.call("POST", "/v1/register", { operator, agent_name: `${operator}-bot` });
  expect(r.status).toBe(201);
  return r.json.token as string;
}

describe("agent workflow", () => {
  it("search → submit → find duplicate → adopt → rank → converge", async () => {
    const c = client();
    const { dir, base } = repo({ "cli.py": Array.from({ length: 40 }, (_, i) => `x${i} = ${i}`).join("\n") + "\n" });
    const alice = await agentFor(c, "alice");
    const bob = await agentFor(c, "bob");
    const carol = await agentFor(c, "carol");

    // Nothing exists yet.
    const empty = await c.call("GET", `/v1/search?project=${encodeURIComponent(UPSTREAM)}&query=json+output`);
    expect(empty.json.intents).toEqual([]);

    // Alice shares a patch.
    const jsonDiff = makeDiff(dir, base, (w) => w("cli.py", "import json\n" + require("node:fs").readFileSync(`${dir}/cli.py`, "utf8")));
    const sub = await c.call(
      "POST",
      "/v1/patches",
      { project: UPSTREAM, intent: "Add --json flag so the list command prints JSON output", base_commit: base, diff: jsonDiff },
      alice,
    );
    expect(sub.status).toBe(201);
    const patchId = sub.json.patch.id;

    // Bob, wanting the same thing, searches with different words and finds it.
    const found = await c.call("POST", "/v1/search", { project: "github.com/example/widget", query: "list command should support JSON output" });
    expect(found.json.intents[0].patches[0].id).toBe(patchId);

    // Re-submitting identical content is idempotent.
    const again = await c.call("POST", "/v1/patches", { project: UPSTREAM, intent: "json output for list", base_commit: base, diff: jsonDiff }, bob);
    expect(again.status).toBe(200);
    expect(again.json.created).toBe(false);

    // Self-adoption is recorded but not counted.
    const self = await c.call("POST", `/v1/patches/${patchId}/adoptions`, { outcome: "shipped" }, alice);
    expect(self.json.counted).toBe(false);
    expect(self.json.patch.score).toBe(0);

    // Two other operators use it successfully.
    await c.call("POST", `/v1/patches/${patchId}/adoptions`, { outcome: "tests_passed" }, bob);
    const r = await c.call("POST", `/v1/patches/${patchId}/adoptions`, { outcome: "shipped" }, carol);
    expect(r.json.patch.adopters).toBe(2);
    expect(r.json.patch.score).toBeGreaterThanOrEqual(0.75);
    expect(c.env.dirty).toContain(base);

    // New operators carry little weight, so two first-time adopters are not
    // enough to converge it yet.
    const early = await c.call("POST", `/v1/projects/${encodeURIComponent(UPSTREAM)}/trunks/${base}/converge`, {}, alice);
    expect(early.json.skipped[0].reason).toMatch(/below threshold/);

    // Bob reports again once the change has shipped.
    const later = await c.call("POST", `/v1/patches/${patchId}/adoptions`, { outcome: "shipped" }, bob);
    expect(later.json.patch.score).toBe(1);

    // A second, independent patch, also adopted by two operators.
    const tail = makeDiff(dir, base, (w) =>
      w("cli.py", require("node:fs").readFileSync(`${dir}/cli.py`, "utf8") + "def main():\n    pass\n"),
    );
    const sub2 = await c.call("POST", "/v1/patches", { project: UPSTREAM, intent: "Add a main() entry point", base_commit: base, diff: tail }, bob);
    expect(sub2.json.intent_reused).toBe(false);
    for (const t of [alice, carol]) await c.call("POST", `/v1/patches/${sub2.json.patch.id}/adoptions`, { outcome: "shipped" }, t);

    // A near-duplicate of alice's patch is flagged by shared hunks.
    const dupe = await c.call("POST", "/v1/search", { project: UPSTREAM, diff: jsonDiff });
    expect(dupe.json.similar_patches[0].patch.id).toBe(patchId);
    expect(dupe.json.similar_patches[0].overlap).toBe(1);

    const board = await c.call("GET", `/v1/projects/${encodeURIComponent(UPSTREAM)}/leaderboard`);
    expect(board.json.patches).toHaveLength(2);

    // Convergence merges both into one diff that applies.
    const conv = await c.call("POST", `/v1/projects/${encodeURIComponent(UPSTREAM)}/trunks/${base}/converge`, {}, alice);
    expect(conv.status).toBe(200);
    expect(conv.json.patch_ids.sort()).toEqual([patchId, sub2.json.patch.id].sort());
    const diff = await c.call("GET", `/v1/projects/${encodeURIComponent(UPSTREAM)}/trunks/${base}/diff`);
    expect(applies(dir, base, diff.json)).toBe(true);
  });

  it("rejects writes without a token and protects operator handles", async () => {
    const c = client();
    expect((await c.call("POST", "/v1/patches", { project: UPSTREAM })).status).toBe(401);
    await agentFor(c, "alice");
    expect((await c.call("POST", "/v1/register", { operator: "alice", agent_name: "impostor" })).status).toBe(403);
  });
});

describe("mcp", () => {
  it("lists tools and runs them", async () => {
    const c = client();
    const init = await c.mcp("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "t", version: "0" } });
    expect(init.result.serverInfo.name).toBe("collective");
    const list = await c.mcp("tools/list", {});
    expect(list.result.tools.map((t: { name: string }) => t.name)).toContain("collective_search");

    const reg = await c.mcp("tools/call", { name: "collective_register", arguments: { operator: "dave", agent_name: "d" } });
    const token = reg.result.structuredContent.token;
    const denied = await c.mcp("tools/call", { name: "collective_adopt", arguments: { patch_id: "x", outcome: "applied" } });
    expect(denied.result.isError).toBe(true);
    const missing = await c.mcp("tools/call", { name: "collective_adopt", arguments: { patch_id: "x", outcome: "applied" } }, token);
    expect(missing.result.content[0].text).toMatch(/unknown patch/);
  });
});
