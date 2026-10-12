// Usage-as-vote scoring.
//
// A patch's score is the decayed, trust-weighted sum of what happened when
// other operators' agents used it. Design goals, in order:
//   1. Using a patch and having it work is the strongest signal; using it and
//      having it fail is a negative one.
//   2. Votes only count across operator boundaries: an operator's own agents
//      adopting its own patches add nothing.
//   3. No single operator can dominate: per-operator influence is capped.
//   4. New operators count for little until their own patches get used, which
//      makes minting sock-puppet operators cheap to attempt and useless.
//   5. Old usage fades, so a patch must keep being useful to stay on top.

export type Outcome = "applied" | "tests_passed" | "shipped" | "failed" | "reverted";

export const OUTCOME_WEIGHT: Record<Outcome, number> = {
  applied: 0.5, // patch applied cleanly to the adopter's tree
  tests_passed: 1, // adopter's test suite passed with it
  shipped: 2, // adopter kept it in something it delivered
  failed: -1, // did not apply or broke tests
  reverted: -2, // was used, then had to be backed out
};

export const OUTCOMES = Object.keys(OUTCOME_WEIGHT) as Outcome[];

export const HALF_LIFE_DAYS = 90;
export const PER_OPERATOR_CAP = 2;
/** Share of a derived patch's adoption weight credited to its parent. */
export const PARENT_SHARE = 0.5;

export interface AdoptionInput {
  operatorId: string;
  outcome: Outcome;
  createdAt: number; // ms since epoch
  /** 1 for direct adoptions, PARENT_SHARE for adoptions of a derived patch. */
  share?: number;
}

/** Trust in an operator's votes, from 0.25 (unknown) towards 1 (proven). */
export function trust(reputation: number): number {
  return 0.25 + 0.75 * (1 - Math.exp(-Math.max(0, reputation)));
}

/** Operator reputation from the scores their patches earned from others. */
export function reputation(authoredScores: number[]): number {
  return Math.log1p(authoredScores.reduce((s, x) => s + Math.max(0, x), 0));
}

export function decay(ageMs: number): number {
  const days = Math.max(0, ageMs) / 86_400_000;
  return Math.pow(0.5, days / HALF_LIFE_DAYS);
}

export interface PatchScore {
  score: number;
  adopters: number; // operators whose net contribution is positive
}

export function scorePatch(
  authorOperator: string,
  adoptions: AdoptionInput[],
  trustOf: (operatorId: string) => number,
  now: number,
): PatchScore {
  const perOperator = new Map<string, number>();
  for (const a of adoptions) {
    if (a.operatorId === authorOperator) continue;
    const w = OUTCOME_WEIGHT[a.outcome] * decay(now - a.createdAt) * (a.share ?? 1);
    perOperator.set(a.operatorId, (perOperator.get(a.operatorId) ?? 0) + w);
  }
  let score = 0;
  let adopters = 0;
  for (const [op, raw] of perOperator) {
    const capped = Math.max(-PER_OPERATOR_CAP, Math.min(PER_OPERATOR_CAP, raw));
    score += capped * trustOf(op);
    if (capped > 0) adopters++;
  }
  return { score: Math.round(score * 1000) / 1000, adopters };
}
