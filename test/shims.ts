// Local stand-ins for the Cloudflare bindings so the service layer can be
// tested in Node against the real schema.
import { DatabaseSync } from "node:sqlite";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import type { Env } from "../src/store";

class Stmt {
  constructor(
    private db: DatabaseSync,
    private sql: string,
    private args: unknown[] = [],
  ) {}
  bind(...args: unknown[]) {
    return new Stmt(this.db, this.sql, args);
  }
  private prep() {
    return this.db.prepare(this.sql);
  }
  async first<T>() {
    return ((this.prep().get(...(this.args as never[])) as T) ?? null) as T | null;
  }
  async all<T>() {
    return { results: this.prep().all(...(this.args as never[])) as T[], success: true };
  }
  async run() {
    this.prep().run(...(this.args as never[]));
    return { success: true };
  }
}

export function d1(): D1Database {
  const db = new DatabaseSync(":memory:");
  const dir = join(import.meta.dirname, "..", "migrations");
  for (const f of readdirSync(dir).sort()) db.exec(readFileSync(join(dir, f), "utf8"));
  return {
    prepare: (sql: string) => new Stmt(db, sql),
    batch: async (stmts: Stmt[]) => {
      db.exec("BEGIN");
      try {
        const out = [];
        for (const s of stmts) out.push(await s.run());
        db.exec("COMMIT");
        return out;
      } catch (e) {
        db.exec("ROLLBACK");
        throw e;
      }
    },
  } as unknown as D1Database;
}

export function r2(): R2Bucket {
  const m = new Map<string, string>();
  return {
    put: async (k: string, v: string) => void m.set(k, v),
    get: async (k: string) => (m.has(k) ? { text: async () => m.get(k)! } : null),
  } as unknown as R2Bucket;
}

export function env(): Env & { dirty: string[] } {
  const dirty: string[] = [];
  const e = { DB: d1(), BLOBS: r2(), dirty } as Env & { dirty: string[] };
  // Minimal Durable Object namespace: RPC calls go straight to the store.
  e.PROJECT = {
    idFromName: (n: string) => n,
    get: () => ({
      markDirty: async (_p: string, base: string) => void dirty.push(base),
      convergeNow: async (p: string, base: string) => {
        const { Store } = await import("../src/store");
        return new Store(e).converge(p, base);
      },
    }),
  } as unknown as DurableObjectNamespace;
  return e;
}
