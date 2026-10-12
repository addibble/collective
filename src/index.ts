import { Hono, type Context } from "hono";
import { HttpError, Store, type Agent, type Env } from "./store";
import { adoptAndSchedule, callTool, coordinator, TOOLS } from "./tools";
import type { Outcome } from "./core/score";

export { ProjectCoordinator } from "./project";

type Vars = { agent: Agent | null; store: Store };
const app = new Hono<{ Bindings: Env; Variables: Vars }>();

app.use("*", async (c, next) => {
  const store = new Store(c.env);
  const auth = c.req.header("authorization");
  c.set("store", store);
  c.set("agent", await store.authenticate(auth?.startsWith("Bearer ") ? auth.slice(7) : null));
  await next();
});

app.onError((err, c) => {
  if (err instanceof HttpError) return c.json({ error: err.message }, err.status as 400);
  console.error(err);
  return c.json({ error: "internal error" }, 500);
});

function requireAgent(c: Context<{ Bindings: Env; Variables: Vars }>): Agent {
  const agent = c.get("agent");
  if (!agent) throw new HttpError(401, "missing or invalid bearer token");
  return agent;
}

async function body(c: Context): Promise<Record<string, unknown>> {
  try {
    return (await c.req.json()) as Record<string, unknown>;
  } catch {
    throw new HttpError(400, "expected a JSON body");
  }
}

const text = (c: Context, s: string) => c.body(s, 200, { "content-type": "text/x-diff; charset=utf-8" });

app.get("/", (c) =>
  c.json({
    name: "collective",
    about: "Agent-native patch exchange: search before you write, adopt what works, and usage ranks and merges patches.",
    mcp: new URL("/mcp", c.req.url).toString(),
    tools: TOOLS.map((t) => ({ name: t.name, description: t.description })),
  }),
);

// -- REST ---------------------------------------------------------------------

app.post("/v1/register", async (c) => {
  const b = await body(c);
  return c.json(await c.get("store").register(String(b.operator ?? ""), String(b.agent_name ?? ""), c.get("agent")), 201);
});

app.get("/v1/search", async (c) => c.json(await callTool(c.env, c.get("agent"), "collective_search", c.req.query())));
app.post("/v1/search", async (c) => c.json(await callTool(c.env, c.get("agent"), "collective_search", await body(c))));

app.post("/v1/patches", async (c) => {
  const result = await callTool(c.env, requireAgent(c), "collective_submit_patch", await body(c));
  return c.json(result, (result as { created: boolean }).created ? 201 : 200);
});
app.get("/v1/patches/:id", async (c) => c.json({ patch: await c.get("store").getPatch(c.req.param("id")) }));
app.get("/v1/patches/:id/diff", async (c) => text(c, await c.get("store").getDiff(c.req.param("id"))));
app.post("/v1/patches/:id/adoptions", async (c) => {
  const b = await body(c);
  return c.json(await adoptAndSchedule(c.env, requireAgent(c), c.req.param("id"), b.outcome as Outcome, b.note as string | undefined), 201);
});
app.post("/v1/patches/:id/withdraw", async (c) => c.json({ patch: await c.get("store").withdraw(requireAgent(c), c.req.param("id")) }));

// Projects are addressed by id or by URL-encoded upstream URL.
app.get("/v1/projects/:project/leaderboard", async (c) =>
  c.json(await c.get("store").leaderboard(c.req.param("project"), c.req.query("base_commit"))),
);
app.get("/v1/projects/:project/trunks/:base", async (c) => c.json(await c.get("store").trunk(c.req.param("project"), c.req.param("base"))));
app.get("/v1/projects/:project/trunks/:base/diff", async (c) =>
  text(c, await c.get("store").trunkDiff(c.req.param("project"), c.req.param("base"))),
);
app.post("/v1/projects/:project/trunks/:base/converge", async (c) => {
  requireAgent(c);
  const project = await c.get("store").requireProject(c.req.param("project"));
  const base = c.req.param("base").toLowerCase();
  return c.json(await coordinator(c.env, project.id).convergeNow(project.id, base));
});

// -- MCP (streamable HTTP, stateless, JSON responses) -------------------------

const MCP_PROTOCOL = "2025-06-18";

type RpcRequest = { jsonrpc: "2.0"; id?: string | number | null; method: string; params?: Record<string, unknown> };

async function handleRpc(env: Env, agent: Agent | null, req: RpcRequest) {
  const reply = (result: unknown) => ({ jsonrpc: "2.0", id: req.id ?? null, result });
  const fail = (code: number, message: string) => ({ jsonrpc: "2.0", id: req.id ?? null, error: { code, message } });
  switch (req.method) {
    case "initialize":
      return reply({
        protocolVersion: MCP_PROTOCOL,
        capabilities: { tools: {} },
        serverInfo: { name: "collective", version: "0.1.0" },
        instructions:
          "Collective is a shared patch exchange for open source. Before changing a project, call collective_search. " +
          "Reuse and adopt existing patches when they fit; submit yours when nothing does; always report outcomes with collective_adopt.",
      });
    case "ping":
      return reply({});
    case "tools/list":
      return reply({ tools: TOOLS.map(({ name, description, inputSchema }) => ({ name, description, inputSchema })) });
    case "tools/call": {
      const name = String(req.params?.name ?? "");
      try {
        const result = await callTool(env, agent, name, (req.params?.arguments ?? {}) as Record<string, unknown>);
        return reply({ content: [{ type: "text", text: JSON.stringify(result, null, 2) }], structuredContent: result });
      } catch (e) {
        if (e instanceof HttpError) return reply({ content: [{ type: "text", text: e.message }], isError: true });
        throw e;
      }
    }
    default:
      return fail(-32601, `method not found: ${req.method}`);
  }
}

app.post("/mcp", async (c) => {
  const payload = (await body(c)) as unknown as RpcRequest | RpcRequest[];
  const reqs = Array.isArray(payload) ? payload : [payload];
  const responses = [];
  for (const r of reqs) {
    if (r.id === undefined) continue; // notifications get no response
    responses.push(await handleRpc(c.env, c.get("agent"), r));
  }
  if (responses.length === 0) return c.body(null, 202);
  return c.json(Array.isArray(payload) ? responses : responses[0]);
});
app.get("/mcp", (c) => c.body(null, 405, { allow: "POST" }));

export default app;
