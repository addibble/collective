// One Durable Object per project. It is the project's single writer for
// convergence: adoptions mark a base commit dirty, and an alarm rebuilds the
// trunk for every dirty base once activity settles. Running it here, rather
// than in the request path, means concurrent adoptions never race to write
// the trunk and a burst of votes costs one rebuild.

import { DurableObject } from "cloudflare:workers";
import { Store, type Env } from "./store";

export const CONVERGE_DEBOUNCE_MS = 30_000;

export class ProjectCoordinator extends DurableObject<Env> {
  private chain: Promise<unknown> = Promise.resolve();

  /** Serializes convergence runs; D1/R2 awaits would otherwise let them interleave. */
  private exclusive<T>(fn: () => Promise<T>): Promise<T> {
    const run = this.chain.then(fn, fn);
    this.chain = run.catch(() => undefined);
    return run;
  }

  async markDirty(projectId: string, baseCommit: string): Promise<void> {
    await this.ctx.storage.put("project", projectId);
    await this.ctx.storage.put(`dirty:${baseCommit}`, true);
    if ((await this.ctx.storage.getAlarm()) === null) {
      await this.ctx.storage.setAlarm(Date.now() + CONVERGE_DEBOUNCE_MS);
    }
  }

  async convergeNow(projectId: string, baseCommit: string) {
    await this.ctx.storage.delete(`dirty:${baseCommit}`);
    return this.exclusive(() => new Store(this.env).converge(projectId, baseCommit));
  }

  async alarm(): Promise<void> {
    const projectId = await this.ctx.storage.get<string>("project");
    if (!projectId) return;
    const dirty = await this.ctx.storage.list({ prefix: "dirty:" });
    for (const key of dirty.keys()) {
      await this.convergeNow(projectId, key.slice("dirty:".length));
    }
  }
}
