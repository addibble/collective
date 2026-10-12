// Jury escalation, after Kleros: jurors are drawn with probability
// proportional to staked rep, vote by commit–reveal so nobody can copy the
// crowd, and are paid for agreeing with the final majority (a Schelling
// point) out of the stakes of those who did not.

import { sha256Hex } from "./diff";
import type { LedgerEntry, Verdict } from "./challenge";

export const JURY = {
  baseSize: 3,
  maxAppeals: 3, // after this, the platform's security council decides
  /** Share of a juror's locked stake lost for voting against the majority or not revealing. */
  penalty: 0.2,
  /** Stake a drawn juror locks per seat. */
  seatStake: 20,
};

/** Kleros-style growth: each appeal draws a jury of twice the size plus one. */
export function jurySize(appeal: number): number {
  let n = JURY.baseSize;
  for (let i = 0; i < appeal; i++) n = 2 * n + 1;
  return n;
}

/** Deterministic PRNG (splitmix32) so a draw can be re-verified from its seed. */
function prng(seedHex: string) {
  let s = parseInt(seedHex.slice(0, 8), 16) >>> 0;
  return () => {
    s = (s + 0x9e3779b9) >>> 0;
    let z = s;
    z = Math.imul(z ^ (z >>> 16), 0x85ebca6b);
    z = Math.imul(z ^ (z >>> 13), 0xc2b2ae35);
    return ((z ^ (z >>> 16)) >>> 0) / 2 ** 32;
  };
}

/**
 * Draws `size` distinct jurors weighted by stake. `exclude` must hold every
 * operator with an interest in the outcome: the author, stakers, attesters
 * and adopters. `seed` should be committed before the candidate list is
 * fixed (e.g. the hash of a secret revealed after the draw) so neither side
 * can grind it.
 */
export async function drawJury(
  candidates: { operatorId: string; stake: number }[],
  size: number,
  seed: string,
  exclude: Set<string>,
): Promise<string[]> {
  const pool = candidates.filter((c) => c.stake >= JURY.seatStake && !exclude.has(c.operatorId));
  if (pool.length < size) throw new Error(`need ${size} eligible jurors, have ${pool.length}`);
  const rand = prng(await sha256Hex(seed));
  const chosen: string[] = [];
  while (chosen.length < size) {
    const total = pool.reduce((s, c) => s + c.stake, 0);
    let r = rand() * total;
    const i = pool.findIndex((c) => (r -= c.stake) < 0);
    chosen.push(pool.splice(i === -1 ? pool.length - 1 : i, 1)[0]!.operatorId);
  }
  return chosen;
}

export function commitVote(operatorId: string, vote: Verdict, salt: string): Promise<string> {
  return sha256Hex(`${operatorId}:${vote}:${salt}`);
}

export async function verifyReveal(commitment: string, operatorId: string, vote: Verdict, salt: string): Promise<boolean> {
  return (await commitVote(operatorId, vote, salt)) === commitment;
}

/**
 * Tallies revealed votes (null = never revealed). A tie returns no verdict,
 * which means appeal or, past maxAppeals, the council.
 */
export function tally(jurors: { operatorId: string; vote: Verdict | null }[]): { verdict: Verdict | null; entries: LedgerEntry[] } {
  const clean = jurors.filter((j) => j.vote === "clean").length;
  const problem = jurors.filter((j) => j.vote === "problem").length;
  const verdict: Verdict | null = clean === problem ? null : clean > problem ? "clean" : "problem";
  const entries: LedgerEntry[] = [];
  if (!verdict) {
    // Nobody is coherent with a tie; only non-revealers are penalized, and their stake goes to the pool.
    for (const j of jurors) if (j.vote === null) entries.push({ operatorId: j.operatorId, delta: -JURY.seatStake * JURY.penalty, reason: "juror did not reveal" });
    return { verdict, entries };
  }
  const coherent = jurors.filter((j) => j.vote === verdict);
  const incoherent = jurors.filter((j) => j.vote !== verdict);
  const forfeited = incoherent.length * JURY.seatStake * JURY.penalty;
  for (const j of incoherent)
    entries.push({ operatorId: j.operatorId, delta: -JURY.seatStake * JURY.penalty, reason: j.vote ? "juror voted against the majority" : "juror did not reveal" });
  for (const j of coherent) entries.push({ operatorId: j.operatorId, delta: forfeited / coherent.length, reason: "coherent juror" });
  return { verdict, entries };
}
