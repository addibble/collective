// MinHash sketches with locality-sensitive hashing (LSH) bands, for
// near-duplicate patch detection.
//
// Exact hunk fingerprints only catch identical edits. Agents produce many
// *almost* identical edits: a renamed variable, one extra line, a different
// error message. MinHash estimates the Jaccard similarity of two patches'
// feature sets from fixed-size signatures, and LSH turns "find similar
// signatures" into a handful of exact key lookups. That is what makes it
// scale: each patch writes BANDS keys, and each query reads BANDS keys,
// regardless of corpus size, so the index can live in any sharded key-value
// or log-structured store.

import { filePath, type FileDiff } from "./diff";

export const PERMUTATIONS = 32;
export const BANDS = 8;
export const ROWS = PERMUTATIONS / BANDS; // 4: candidate probability ≈ 1 − (1 − s⁴)⁸

/** 32-bit FNV-1a. */
function fnv1a(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

/** murmur3 finalizer: decorrelates the per-permutation hashes. */
function mix(h: number): number {
  h ^= h >>> 16;
  h = Math.imul(h, 0x85ebca6b);
  h ^= h >>> 13;
  h = Math.imul(h, 0xc2b2ae35);
  h ^= h >>> 16;
  return h >>> 0;
}

const SEEDS = Array.from({ length: PERMUTATIONS }, (_, i) => mix(0x9e3779b9 ^ Math.imul(i + 1, 0x7f4a7c15)));

/**
 * Features of a patch: each changed line (normalized as for fingerprints)
 * and each pair of consecutive changed lines, so order matters a little.
 * The file's base name is folded in so identical one-liners in unrelated
 * files don't collide, while moves between directories still match.
 */
export function features(files: FileDiff[]): string[] {
  const out = new Set<string>();
  for (const f of files) {
    const base = filePath(f).split("/").pop()!;
    for (const h of f.hunks) {
      let prev: string | null = null;
      for (const l of h.lines) {
        if (l[0] !== "+" && l[0] !== "-") continue;
        const norm = l[0] + l.slice(1).trim().replace(/\s+/g, " ");
        if (norm.length <= 1) continue;
        out.add(`${base}\0${norm}`);
        if (prev) out.add(`${base}\0${prev}\0${norm}`);
        prev = norm;
      }
    }
  }
  return [...out];
}

export function signature(feats: string[]): Uint32Array {
  const sig = new Uint32Array(PERMUTATIONS).fill(0xffffffff);
  for (const f of feats) {
    const h = fnv1a(f);
    for (let i = 0; i < PERMUTATIONS; i++) {
      const v = mix(h ^ SEEDS[i]!);
      if (v < sig[i]!) sig[i] = v;
    }
  }
  return sig;
}

export function encodeSignature(sig: Uint32Array): string {
  return [...sig].map((v) => v.toString(16).padStart(8, "0")).join("");
}

export function decodeSignature(hex: string): Uint32Array {
  const sig = new Uint32Array(PERMUTATIONS);
  for (let i = 0; i < PERMUTATIONS; i++) sig[i] = parseInt(hex.slice(i * 8, i * 8 + 8), 16);
  return sig;
}

/** LSH band keys. Two patches become candidates if any band key matches. */
export function bands(sig: Uint32Array): string[] {
  const out: string[] = [];
  for (let b = 0; b < BANDS; b++) {
    let h = 0x811c9dc5 ^ b;
    for (let r = 0; r < ROWS; r++) h = mix(h ^ sig[b * ROWS + r]!);
    out.push(`${b}:${h.toString(16).padStart(8, "0")}`);
  }
  return out;
}

/** Estimated Jaccard similarity of the underlying feature sets. */
export function estimateJaccard(a: Uint32Array, b: Uint32Array): number {
  let eq = 0;
  for (let i = 0; i < PERMUTATIONS; i++) if (a[i] === b[i]) eq++;
  return eq / PERMUTATIONS;
}

export interface PatchSketch {
  signature: string;
  bands: string[];
}

export function sketch(files: FileDiff[]): PatchSketch | null {
  const feats = features(files);
  if (feats.length === 0) return null;
  const sig = signature(feats);
  return { signature: encodeSignature(sig), bands: bands(sig) };
}
