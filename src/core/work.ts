// Hashcash-style proof of work, used only as an entry ticket for operators
// with no rep to stake. It rate-limits anonymous reports. It cannot make
// a report honest: agents have cheap compute, and the work costs a liar the
// same as a truth-teller. Stake does that. See docs/CHALLENGES.md.

import { sha256Hex } from "./diff";

/** Difficulty for a stake-less report: about 2^20 hashes (a second or so of CPU). */
export const REPORT_BITS = 20;

function leadingZeroBits(hex: string): number {
  let bits = 0;
  for (const ch of hex) {
    const n = parseInt(ch, 16);
    if (n === 0) {
      bits += 4;
      continue;
    }
    return bits + Math.clz32(n) - 28;
  }
  return bits;
}

/** `resource` must bind the work to one report: operator, patch, evidence commitment and a recent timestamp. */
export async function verifyWork(resource: string, nonce: string, bits: number): Promise<boolean> {
  return leadingZeroBits(await sha256Hex(`${resource}:${nonce}`)) >= bits;
}

export async function solveWork(resource: string, bits: number): Promise<string> {
  for (let i = 0; ; i++) if (await verifyWork(resource, i.toString(36), bits)) return i.toString(36);
}
