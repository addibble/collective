// Challenge protocol: flag a patch as harmful, escalate with bonds, settle
// with slashing and a bounty. See docs/CHALLENGES.md for the rationale and
// the protocols it borrows from (Augur, UMA, Kleros, Truebit).
//
// Units are reputation points ("rep"): non-transferable stake earned by
// contributing patches that others use and by winning challenges. Every
// staked amount is debited when staked; settlement emits credits.
//
// The staking game is binary: "is there a real problem in this patch?"
// Severity and intent (malicious vs accidental) are decided when it
// resolves, so a finder never loses for picking the wrong label for a real
// bug.

export type Verdict = "clean" | "problem";
export type Severity = "low" | "medium" | "high" | "critical";
export type Intent = "malicious" | "accidental" | "unknown";
export type ResolvedVia = "proof" | "timeout" | "jury" | "council";

export const PARAMS = {
  /** Smallest bond that quarantines a patch on flagging. */
  minFlagBond: 10,
  /** How long each staking round stays open. Agents respond in minutes; humans need a day. */
  roundMs: 24 * 3600_000,
  /** Total stake at which the dispute leaves the bond game for a jury. */
  juryThreshold: 2_000,
  maxRounds: 6,
  /** Share of the losing side's stake withheld for the bounty pool and jurors. */
  fee: 0.1,
  /** Share of the bounty paid to the first flagger; the rest goes to co-backers. */
  finderShare: 0.8,
  /** Rep lost per unit of adoption weight by operators who vouched for a problem patch. */
  adopterSlash: 2,
  /** Share of the author's balance slashed, by intent. */
  authorSlash: { malicious: 1, unknown: 0.25, accidental: 0.1 } as Record<Intent, number>,
  /**
   * Bounty paid from the platform pool, multiplied by ln(1 + independent
   * adopters). A patch nobody else used harmed nobody: finding it pays only
   * the slashed stakes, so planting a bug with one identity and "finding" it
   * with another earns nothing from the pool.
   */
  severityBounty: { low: 20, medium: 100, high: 500, critical: 2_500 } as Record<Severity, number>,
};

export interface Contribution {
  operatorId: string;
  side: Verdict;
  amount: number;
  round: number;
  at: number;
}

export interface Challenge {
  id: string;
  patchId: string;
  flaggedBy: string;
  severity: Severity; // as claimed; may be revised at resolution
  /** sha256 of the evidence and a salt, so the finder's priority is fixed before the evidence is revealed. */
  evidenceCommitment: string;
  openedAt: number;
  contributions: Contribution[];
  /** (round, side) pairs whose bond was filled; only filled stakes are at risk. */
  filled: string[];
  round: number;
  tentative: Verdict;
  roundEndsAt: number;
  phase: "staking" | "jury" | "resolved";
  resolution?: { verdict: Verdict; via: ResolvedVia; severity: Severity; intent: Intent; at: number };
}

export class ChallengeError extends Error {}

const key = (round: number, side: Verdict) => `${round}:${side}`;
const other = (v: Verdict): Verdict => (v === "clean" ? "problem" : "clean");

function filledTotal(c: Challenge, side?: Verdict): number {
  return c.contributions
    .filter((x) => c.filled.includes(key(x.round, x.side)) && (!side || x.side === side))
    .reduce((s, x) => s + x.amount, 0);
}

/** Whether a patch should be held out of trunks and shown with a warning. */
export function quarantined(c: Challenge): boolean {
  return c.phase === "resolved" ? c.resolution!.verdict === "problem" : c.tentative === "problem";
}

/**
 * Opens a challenge. With a bond of at least minFlagBond the patch is
 * quarantined at once (the flagger acts as Augur's "designated reporter").
 * With no bond, which callers allow only after a proof of work, the report
 * is recorded but the patch stays live until someone stakes on it.
 */
export function openChallenge(input: {
  id: string;
  patchId: string;
  flaggedBy: string;
  severity: Severity;
  evidenceCommitment: string;
  bond: number;
  now: number;
}): Challenge {
  if (input.bond < 0) throw new ChallengeError("bond must be non-negative");
  if (input.bond > 0 && input.bond < PARAMS.minFlagBond) throw new ChallengeError(`a bond must be at least ${PARAMS.minFlagBond}`);
  const bonded = input.bond > 0;
  return {
    id: input.id,
    patchId: input.patchId,
    flaggedBy: input.flaggedBy,
    severity: input.severity,
    evidenceCommitment: input.evidenceCommitment,
    openedAt: input.now,
    contributions: bonded ? [{ operatorId: input.flaggedBy, side: "problem", amount: input.bond, round: 0, at: input.now }] : [],
    filled: bonded ? [key(0, "problem")] : [],
    round: 0,
    tentative: bonded ? "problem" : "clean",
    roundEndsAt: input.now + PARAMS.roundMs,
    phase: "staking",
  };
}

/**
 * Stake needed to overturn the tentative outcome in the current round:
 * Augur's escalating bond, 2·(total at stake) − 3·(already staked on the
 * challenger's side). Each overturn roughly doubles the stake, so a dispute
 * either settles quickly or becomes expensive enough to justify a jury.
 */
export function bondToFlip(c: Challenge): number {
  const side = other(c.tentative);
  return Math.max(PARAMS.minFlagBond, 2 * filledTotal(c) - 3 * filledTotal(c, side));
}

/** Contributed so far in this round towards overturning. */
export function pendingFor(c: Challenge): number {
  const side = other(c.tentative);
  return c.contributions.filter((x) => x.round === c.round && x.side === side).reduce((s, x) => s + x.amount, 0);
}

/**
 * Adds stake against the tentative outcome. Anyone may contribute part of
 * the bond (crowdsourced, as in Augur). Returns the updated challenge and
 * the amount actually accepted; any excess over the bond is not taken.
 */
export function contribute(c: Challenge, operatorId: string, side: Verdict, amount: number, now: number): { challenge: Challenge; accepted: number } {
  c = advance(c, now);
  if (c.phase !== "staking") throw new ChallengeError(`challenge is ${c.phase}`);
  if (side === c.tentative) throw new ChallengeError(`"${side}" is already the tentative outcome; stake against it or wait`);
  if (!(amount > 0)) throw new ChallengeError("amount must be positive");

  const need = bondToFlip(c) - pendingFor(c);
  const accepted = Math.min(amount, need);
  const next: Challenge = structuredClone(c);
  next.contributions.push({ operatorId, side, amount: accepted, round: c.round, at: now });

  if (accepted === need) {
    next.filled.push(key(c.round, side));
    next.tentative = side;
    next.round = c.round + 1;
    next.roundEndsAt = now + PARAMS.roundMs;
    if (filledTotal(next) >= PARAMS.juryThreshold || next.round >= PARAMS.maxRounds) next.phase = "jury";
  }
  return { challenge: next, accepted };
}

/** Resolves by timeout if the current round expired without being overturned. */
export function advance(c: Challenge, now: number): Challenge {
  if (c.phase !== "staking" || now < c.roundEndsAt) return c;
  return resolve(c, { verdict: c.tentative, via: "timeout", severity: c.severity, intent: "unknown", at: c.roundEndsAt });
}

/**
 * Final resolution. A verified proof (a test that fails with the patch and
 * passes without it, run by the platform's sandbox) resolves at any time,
 * like a fraud proof in an optimistic rollup: objective evidence beats stake.
 */
export function resolve(c: Challenge, r: NonNullable<Challenge["resolution"]>): Challenge {
  if (c.phase === "resolved") throw new ChallengeError("already resolved");
  if (r.via === "timeout" && c.phase !== "staking") throw new ChallengeError("only the staking phase resolves by timeout");
  if (r.verdict === "clean" && r.intent !== "unknown") r = { ...r, intent: "unknown" };
  return { ...structuredClone(c), phase: "resolved", resolution: r };
}

// ---------------------------------------------------------------------------
// Settlement
// ---------------------------------------------------------------------------

export interface LedgerEntry {
  operatorId: string;
  delta: number;
  reason: string;
}

export interface SettlementContext {
  authorOperator: string;
  authorBalance: number;
  /** Operators that adopted the patch with a positive outcome, and their adoption weight. */
  adopters: { operatorId: string; weight: number }[];
  /** Standing "I reviewed this, it's clean" stakes on the patch. */
  attestations: { operatorId: string; amount: number }[];
  /** Platform bounty pool available for this payout. */
  pool: number;
}

export interface Settlement {
  entries: LedgerEntry[];
  /** Net change to the platform pool (fees and slashes in, bounty out). */
  poolDelta: number;
  bounty: number;
}

const round2 = (x: number) => Math.round(x * 100) / 100;

export function settle(c: Challenge, ctx: SettlementContext): Settlement {
  if (c.phase !== "resolved") throw new ChallengeError("settle after resolution");
  const { verdict, severity, intent } = c.resolution!;
  const entries: LedgerEntry[] = [];
  let pool = 0;

  // 1. Unfilled partial contributions are returned untouched.
  for (const x of c.contributions)
    if (!c.filled.includes(key(x.round, x.side))) entries.push({ operatorId: x.operatorId, delta: x.amount, reason: "refund: bond not filled" });

  // 2. Filled stakes: winners get theirs back plus the losers', less the fee.
  const filled = c.contributions.filter((x) => c.filled.includes(key(x.round, x.side)));
  const winners = filled.filter((x) => x.side === verdict);
  const losing = filled.filter((x) => x.side !== verdict).reduce((s, x) => s + x.amount, 0);
  const winning = winners.reduce((s, x) => s + x.amount, 0);
  const fee = losing * PARAMS.fee;
  pool += fee;
  for (const x of winners) {
    entries.push({ operatorId: x.operatorId, delta: round2(x.amount + ((losing - fee) * x.amount) / winning), reason: "stake returned with winnings" });
  }
  if (winning === 0) pool += losing - fee; // nobody backed the outcome (e.g. a proof overturned a bare report)

  if (verdict === "clean") return { entries, poolDelta: round2(pool), bounty: 0 };

  // 3. A real problem: slash those who vouched for it, then pay the bounty.
  let slashed = 0;
  for (const a of ctx.attestations) {
    entries.push({ operatorId: a.operatorId, delta: -a.amount, reason: "attested clean on a problem patch" });
    slashed += a.amount;
  }
  for (const a of ctx.adopters) {
    if (a.operatorId === ctx.authorOperator) continue;
    const amount = round2(PARAMS.adopterSlash * a.weight);
    entries.push({ operatorId: a.operatorId, delta: -amount, reason: "adopted a problem patch" });
    slashed += amount;
  }
  const authorLoss = round2(ctx.authorBalance * PARAMS.authorSlash[intent]);
  if (authorLoss > 0) {
    entries.push({ operatorId: ctx.authorOperator, delta: -authorLoss, reason: `authored a problem patch (${intent})` });
    slashed += authorLoss;
  }

  const exposure = ctx.adopters.filter((a) => a.operatorId !== ctx.authorOperator).length;
  const fromPool = Math.min(ctx.pool + pool, PARAMS.severityBounty[severity] * Math.log1p(exposure));
  const bounty = round2(slashed + fromPool);
  pool -= fromPool;

  const backers = winners.filter((x) => x.operatorId !== c.flaggedBy);
  const backerTotal = backers.reduce((s, x) => s + x.amount, 0);
  const finderCut = backerTotal > 0 ? PARAMS.finderShare : 1;
  entries.push({ operatorId: c.flaggedBy, delta: round2(bounty * finderCut), reason: `bounty: ${severity} problem found` });
  for (const x of backers) {
    entries.push({ operatorId: x.operatorId, delta: round2((bounty * (1 - finderCut) * x.amount) / backerTotal), reason: "bounty share for backing the finding" });
  }
  return { entries, poolDelta: round2(pool), bounty };
}
