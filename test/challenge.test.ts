import { describe, expect, it } from "vitest";
import { advance, bondToFlip, contribute, openChallenge, PARAMS, quarantined, resolve, settle, type Challenge } from "../src/core/challenge";
import { commitVote, drawJury, jurySize, tally, verifyReveal } from "../src/core/jury";
import { solveWork, verifyWork } from "../src/core/work";

const T0 = 1_000_000;
const DAY = PARAMS.roundMs;

const open = (bond: number) =>
  openChallenge({ id: "c1", patchId: "p1", flaggedBy: "finder", severity: "high", evidenceCommitment: "abc", bond, now: T0 });

const ctx = {
  authorOperator: "author",
  authorBalance: 100,
  adopters: [
    { operatorId: "a1", weight: 1 },
    { operatorId: "a2", weight: 2 },
  ],
  attestations: [{ operatorId: "rubberstamp", amount: 50 }],
  pool: 10_000,
};

const sum = (entries: { operatorId: string; delta: number }[], op: string) =>
  entries.filter((e) => e.operatorId === op).reduce((s, e) => s + e.delta, 0);

describe("challenge staking game", () => {
  it("quarantines on a bonded flag but not on a bare report", () => {
    expect(quarantined(open(10))).toBe(true);
    expect(quarantined(open(0))).toBe(false);
    expect(() => open(5)).toThrow(/at least/);
  });

  it("escalates with Augur-style bonds and crowdsourced contributions", () => {
    let c = open(10);
    expect(bondToFlip(c)).toBe(20); // 2·10 − 3·0
    let r = contribute(c, "author", "clean", 15, T0 + 1);
    expect(r.accepted).toBe(15);
    expect(r.challenge.tentative).toBe("problem"); // not filled yet
    r = contribute(r.challenge, "friend", "clean", 100, T0 + 2);
    expect(r.accepted).toBe(5); // only the remainder is taken
    c = r.challenge;
    expect(c.tentative).toBe("clean");
    expect(quarantined(c)).toBe(false);
    // Total 30, 10 already on "problem": 2·30 − 3·10 = 30.
    expect(bondToFlip(c)).toBe(30);
  });

  it("refuses to stake on the side that is already winning", () => {
    expect(() => contribute(open(10), "x", "problem", 10, T0)).toThrow(/tentative/);
  });

  it("resolves by timeout and settles a clean verdict against the flagger", () => {
    let c = contribute(open(10), "author", "clean", 20, T0 + 1).challenge;
    c = contribute(c, "finder", "problem", 7, T0 + 2).challenge; // partial, never filled
    c = advance(c, T0 + 2 + DAY);
    expect(c.resolution).toMatchObject({ verdict: "clean", via: "timeout" });
    const s = settle(c, ctx);
    expect(s.bounty).toBe(0);
    expect(sum(s.entries, "author")).toBe(29); // 20 back + 10 lost by the flagger − 10% fee
    expect(sum(s.entries, "finder")).toBe(7); // only the unfilled partial comes back
    expect(s.poolDelta).toBe(1);
    expect(sum(s.entries, "rubberstamp")).toBe(0);
  });

  it("goes to a jury once enough is at stake", () => {
    let c: Challenge = open(1000);
    c = contribute(c, "author", "clean", bondToFlip(c), T0 + 1).challenge;
    expect(c.phase).toBe("jury");
    expect(() => contribute(c, "x", "problem", 10, T0 + 2)).toThrow(/jury/);
  });

  it("lets a proof overturn stake and pays the finder from slashes and the pool", () => {
    let c = contribute(open(10), "author", "clean", 20, T0 + 1).challenge;
    c = resolve(c, { verdict: "problem", via: "proof", severity: "high", intent: "malicious", at: T0 + 5 });
    const s = settle(c, ctx);
    // Author: loses the 20 staked (debited earlier) and all 100 of balance.
    expect(sum(s.entries, "author")).toBe(-100);
    expect(sum(s.entries, "rubberstamp")).toBe(-50);
    expect(sum(s.entries, "a1")).toBe(-2);
    expect(sum(s.entries, "a2")).toBe(-4);
    // Finder: stake back with winnings (10 + 18) plus the whole bounty.
    const fromPool = PARAMS.severityBounty.high * Math.log1p(2);
    expect(s.bounty).toBeCloseTo(156 + fromPool, 1);
    expect(sum(s.entries, "finder")).toBeCloseTo(28 + s.bounty, 1);
  });

  it("pays nothing from the pool for a problem patch nobody adopted", () => {
    const c = resolve(open(10), { verdict: "problem", via: "proof", severity: "critical", intent: "malicious", at: T0 + 5 });
    const s = settle(c, { ...ctx, adopters: [], attestations: [], authorBalance: 30 });
    expect(s.bounty).toBe(30); // the author's slashed balance only
    expect(s.poolDelta).toBe(0);
  });

  it("splits the bounty with those who backed a bare report", () => {
    let c = open(0);
    c = contribute(c, "backer", "problem", bondToFlip(c), T0 + 1).challenge;
    expect(quarantined(c)).toBe(true);
    c = advance(c, T0 + 1 + DAY);
    const s = settle(c, { ...ctx, attestations: [], adopters: [], authorBalance: 0 });
    expect(sum(s.entries, "finder")).toBeCloseTo(s.bounty * PARAMS.finderShare, 1);
    expect(sum(s.entries, "backer")).toBeCloseTo(10 + s.bounty * (1 - PARAMS.finderShare), 1);
  });
});

describe("jury", () => {
  const candidates = Array.from({ length: 30 }, (_, i) => ({ operatorId: `j${i}`, stake: 20 + i * 10 }));

  it("draws deterministically, without interested parties, weighted by stake", async () => {
    const a = await drawJury(candidates, 7, "seed", new Set(["j29"]));
    expect(a).toEqual(await drawJury(candidates, 7, "seed", new Set(["j29"])));
    expect(new Set(a).size).toBe(7);
    expect(a).not.toContain("j29");
    // Over many draws, the largest staker is picked far more often than the smallest.
    const counts = new Map<string, number>();
    for (let i = 0; i < 300; i++) for (const j of await drawJury(candidates, 1, `s${i}`, new Set())) counts.set(j, (counts.get(j) ?? 0) + 1);
    expect(counts.get("j28") ?? 0).toBeGreaterThan(counts.get("j0") ?? 0);
  });

  it("grows 3, 7, 15 on appeal", () => {
    expect([0, 1, 2].map(jurySize)).toEqual([3, 7, 15]);
  });

  it("verifies commit–reveal and pays coherent jurors from the others", async () => {
    const c = await commitVote("j1", "problem", "salt");
    expect(await verifyReveal(c, "j1", "problem", "salt")).toBe(true);
    expect(await verifyReveal(c, "j1", "clean", "salt")).toBe(false);
    const t = tally([
      { operatorId: "j1", vote: "problem" },
      { operatorId: "j2", vote: "problem" },
      { operatorId: "j3", vote: "clean" },
      { operatorId: "j4", vote: null },
    ]);
    expect(t.verdict).toBe("problem");
    expect(sum(t.entries, "j1")).toBe(4);
    expect(sum(t.entries, "j3")).toBe(-4);
    expect(sum(t.entries, "j4")).toBe(-4);
    expect(tally([{ operatorId: "a", vote: "clean" }, { operatorId: "b", vote: "problem" }]).verdict).toBeNull();
  });
});

describe("proof of work", () => {
  it("solves and verifies hashcash bound to a resource", async () => {
    const nonce = await solveWork("op:patch:commitment", 10);
    expect(await verifyWork("op:patch:commitment", nonce, 10)).toBe(true);
    expect(await verifyWork("op:other-patch:commitment", nonce, 10)).toBe(false);
  });
});
