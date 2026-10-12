// Convergence: fold the most-used compatible patches for one base commit into
// a single "trunk" diff. This is the merge step that, in a human-run project,
// waits on a maintainer.
//
// Rules, applied greedily in score order:
//   - only patches above the score and adopter thresholds are considered;
//   - one patch per intent (alternatives implementing the same intent compete,
//     they are not stacked);
//   - a patch is skipped if it derives from, or is derived from, one already
//     chosen (the derived one already contains or replaces it);
//   - a patch is skipped if it touches lines another chosen patch touches.

import { combine, findConflict, formatDiff, type FileDiff } from "./diff";

export interface Candidate {
  id: string;
  intentId: string;
  parentId: string | null;
  score: number;
  adopters: number;
  files: FileDiff[];
}

export interface ConvergeOptions {
  minScore: number;
  minAdopters: number;
}

export const DEFAULT_CONVERGE: ConvergeOptions = { minScore: 1, minAdopters: 2 };

export interface Trunk {
  patchIds: string[];
  skipped: { patchId: string; reason: string }[];
  diff: string;
}

export function converge(candidates: Candidate[], opts: ConvergeOptions = DEFAULT_CONVERGE): Trunk {
  const sorted = [...candidates].sort((a, b) => b.score - a.score || b.adopters - a.adopters || a.id.localeCompare(b.id));
  const parentOf = new Map(candidates.map((c) => [c.id, c.parentId]));
  const lineage = (id: string): Set<string> => {
    const out = new Set<string>();
    for (let p = parentOf.get(id) ?? null; p && !out.has(p); p = parentOf.get(p) ?? null) out.add(p);
    return out;
  };

  const chosen: Candidate[] = [];
  const intents = new Set<string>();
  let accepted: FileDiff[] = [];
  const skipped: Trunk["skipped"] = [];

  for (const c of sorted) {
    if (c.score < opts.minScore || c.adopters < opts.minAdopters) {
      skipped.push({ patchId: c.id, reason: `below threshold (score ${c.score}, adopters ${c.adopters})` });
      continue;
    }
    const related = chosen.find((x) => lineage(c.id).has(x.id) || lineage(x.id).has(c.id));
    if (related) {
      skipped.push({ patchId: c.id, reason: `same lineage as ${related.id}, which ranks higher` });
      continue;
    }
    if (intents.has(c.intentId)) {
      skipped.push({ patchId: c.id, reason: `intent ${c.intentId} already served by a higher-ranked patch` });
      continue;
    }
    const conflict = findConflict(accepted, c.files);
    if (conflict) {
      skipped.push({ patchId: c.id, reason: `conflicts in ${conflict.path}: ${conflict.reason}` });
      continue;
    }
    chosen.push(c);
    intents.add(c.intentId);
    accepted = combine([accepted, c.files]);
  }

  return { patchIds: chosen.map((c) => c.id), skipped, diff: accepted.length ? formatDiff(accepted) : "" };
}
