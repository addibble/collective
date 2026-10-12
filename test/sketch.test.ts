import { describe, expect, it } from "vitest";
import { parseDiff } from "../src/core/diff";
import { bands, decodeSignature, estimateJaccard, features, signature, sketch } from "../src/core/sketch";

function diff(path: string, added: string[]) {
  return parseDiff(
    [`diff --git a/${path} b/${path}`, `--- a/${path}`, `+++ b/${path}`, `@@ -1,1 +1,${added.length + 1} @@`, " ctx", ...added.map((l) => "+" + l)].join("\n") + "\n",
  );
}

const body = Array.from({ length: 20 }, (_, i) => `    out.append(render_row(row, column_${i}))`);

describe("minhash sketches", () => {
  it("rates a lightly edited copy as similar and unrelated code as not", () => {
    const a = sketch(diff("src/list.py", body))!;
    const tweaked = [...body];
    tweaked[7] = "    out.append(render_row(row, col7))";
    const b = sketch(diff("src/list.py", tweaked))!;
    const c = sketch(diff("src/list.py", body.map((l, i) => `    total += weight_${i} * value`)))!;
    const sa = decodeSignature(a.signature);
    expect(estimateJaccard(sa, decodeSignature(b.signature))).toBeGreaterThan(0.6);
    expect(estimateJaccard(sa, decodeSignature(c.signature))).toBeLessThan(0.2);
    // The near-duplicate shares at least one LSH band, so a key lookup finds it.
    expect(a.bands.some((k) => b.bands.includes(k))).toBe(true);
    expect(a.bands.some((k) => c.bands.includes(k))).toBe(false);
  });

  it("ignores whitespace and directory moves", () => {
    const a = sketch(diff("src/list.py", body))!;
    const b = sketch(diff("lib/v2/list.py", body.map((l) => "\t" + l.trim() + "  ")))!;
    expect(a.signature).toBe(b.signature);
  });

  it("estimates Jaccard similarity within sampling error", () => {
    // Two sets sharing 60 of 100 elements each: true Jaccard = 60 / 140 ≈ 0.43.
    const shared = Array.from({ length: 60 }, (_, i) => `s${i}`);
    const x = signature([...shared, ...Array.from({ length: 40 }, (_, i) => `x${i}`)]);
    const y = signature([...shared, ...Array.from({ length: 40 }, (_, i) => `y${i}`)]);
    expect(Math.abs(estimateJaccard(x, y) - 60 / 140)).toBeLessThan(0.2);
    expect(bands(x)).toHaveLength(8);
  });

  it("has no features for whitespace-only changes", () => {
    expect(features(diff("a.txt", ["   ", ""]))).toEqual([]);
  });
});
