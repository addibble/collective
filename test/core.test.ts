import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { combine, findConflict, formatDiff, hunkFingerprints, parseDiff, patchId } from "../src/core/diff";
import { converge } from "../src/core/converge";
import { scorePatch, trust } from "../src/core/score";
import { similarity, terms } from "../src/core/text";
import { applies, git, makeDiff, repo } from "./git";

const lines = (n: number, tag = "line") => Array.from({ length: n }, (_, i) => `${tag} ${i + 1}`).join("\n") + "\n";

function fixture() {
  const { dir, base } = repo({ "src/a.txt": lines(60), "src/b.txt": lines(20, "b") });
  const edit = (path: string, at: number, text: string) =>
    makeDiff(dir, base, (w) => {
      const l = readFileSync(join(dir, path), "utf8").split("\n");
      l[at - 1] = text;
      w(path, l.join("\n"));
    });
  return { dir, base, edit };
}

describe("diff", () => {
  it("round-trips git output and git accepts the re-formatted diff", () => {
    const { dir, base, edit } = fixture();
    const d = edit("src/a.txt", 10, "changed");
    const files = parseDiff(d);
    expect(files).toHaveLength(1);
    expect(applies(dir, base, formatDiff(files))).toBe(true);
  });

  it("handles created and deleted files", () => {
    const { dir, base } = repo({ "old.txt": "bye\n", "keep.txt": "x\n" });
    const d = makeDiff(dir, base, (w) => {
      w("new/file.txt", "hello\nworld\n");
      git(dir, "rm", "-q", "old.txt");
    });
    const files = parseDiff(d);
    expect(files.map((f) => [f.oldPath, f.newPath])).toEqual(
      expect.arrayContaining([
        [null, "new/file.txt"],
        ["old.txt", null],
      ]),
    );
    expect(applies(dir, base, formatDiff(files))).toBe(true);
  });

  it("combines non-overlapping patches into one diff that git applies", () => {
    const { dir, base, edit } = fixture();
    const p1 = parseDiff(edit("src/a.txt", 10, "one"));
    const p2 = parseDiff(
      makeDiff(dir, base, (w) => w("src/a.txt", lines(60).replace("line 40\n", "line 40\ninserted a\ninserted b\n"))),
    );
    const p3 = parseDiff(edit("src/b.txt", 5, "three"));
    expect(findConflict(p1, p2)).toBeNull();
    const merged = formatDiff(combine([p2, p1, p3]));
    expect(applies(dir, base, merged)).toBe(true);
    const a = readFileSync(join(dir, "src/a.txt"), "utf8");
    expect(a).toContain("one\n");
    expect(a).toContain("inserted b\n");
    expect(readFileSync(join(dir, "src/b.txt"), "utf8")).toContain("three\n");
  });

  it("flags overlapping edits as conflicts", () => {
    const { edit } = fixture();
    const p1 = parseDiff(edit("src/a.txt", 10, "one"));
    const p2 = parseDiff(edit("src/a.txt", 12, "two"));
    expect(findConflict(p1, p2)?.path).toBe("src/a.txt");
  });

  it("fingerprints ignore line numbers and whitespace", async () => {
    const { dir, base } = repo({ "f.txt": lines(30) });
    const a = makeDiff(dir, base, (w) => w("f.txt", lines(30).replace("line 5\n", "line 5\n  foo(bar)\n")));
    const b = makeDiff(dir, base, (w) => w("f.txt", lines(30).replace("line 20\n", "line 20\nfoo(bar)\n")));
    const [fa] = await hunkFingerprints(parseDiff(a));
    const [fb] = await hunkFingerprints(parseDiff(b));
    expect(fa!.fingerprint).toBe(fb!.fingerprint);
    expect(await patchId(base, parseDiff(a))).not.toBe(await patchId(base, parseDiff(b)));
  });

  it("rejects garbage", () => {
    expect(() => parseDiff("hello")).toThrow();
  });
});

describe("text", () => {
  it("matches reworded intents above unrelated ones", () => {
    const q = terms("Add a --json flag to the list command output");
    const close = terms("list command: support JSON output format");
    const far = terms("fix crash when config file is missing");
    expect(similarity(q, close)).toBeGreaterThan(similarity(q, far));
    expect(similarity(q, far)).toBe(0);
  });
});

describe("score", () => {
  const t = Date.now();
  const flat = () => trust(0);
  it("ignores the author's own operator", () => {
    expect(scorePatch("me", [{ operatorId: "me", outcome: "shipped", createdAt: t }], flat, t).score).toBe(0);
  });
  it("caps any one operator's influence", () => {
    const spam = Array.from({ length: 50 }, () => ({ operatorId: "x", outcome: "shipped" as const, createdAt: t }));
    const one = scorePatch("me", spam, () => 1, t);
    expect(one.score).toBe(2);
    expect(one.adopters).toBe(1);
  });
  it("counts failures against a patch and decays old usage", () => {
    const s = scorePatch(
      "me",
      [
        { operatorId: "a", outcome: "tests_passed", createdAt: t },
        { operatorId: "b", outcome: "failed", createdAt: t },
        { operatorId: "c", outcome: "tests_passed", createdAt: t - 90 * 86_400_000 },
      ],
      () => 1,
      t,
    );
    expect(s.score).toBeCloseTo(0.5);
    expect(s.adopters).toBe(2);
  });
  it("trusts proven operators more than new ones", () => {
    expect(trust(0)).toBe(0.25);
    expect(trust(5)).toBeGreaterThan(0.99);
  });
});

describe("converge", () => {
  it("stacks compatible winners, one per intent, and explains every skip", () => {
    const { dir, base, edit } = fixture();
    const mk = (id: string, intentId: string, score: number, diff: string, parentId: string | null = null) => ({
      id,
      intentId,
      parentId,
      score,
      adopters: 3,
      files: parseDiff(diff),
    });
    const trunk = converge([
      mk("p-top", "i1", 5, edit("src/a.txt", 10, "top")),
      mk("p-rival", "i1", 4, edit("src/a.txt", 50, "rival")),
      mk("p-clash", "i2", 3, edit("src/a.txt", 11, "clash")),
      mk("p-other", "i3", 2, edit("src/b.txt", 3, "other")),
      mk("p-child", "i4", 1.5, edit("src/a.txt", 30, "child"), "p-top"),
      mk("p-weak", "i5", 0.2, edit("src/a.txt", 55, "weak")),
    ]);
    expect(trunk.patchIds).toEqual(["p-top", "p-other"]);
    const reasons = Object.fromEntries(trunk.skipped.map((s) => [s.patchId, s.reason]));
    expect(reasons["p-rival"]).toMatch(/intent/);
    expect(reasons["p-clash"]).toMatch(/conflicts/);
    expect(reasons["p-child"]).toMatch(/lineage/);
    expect(reasons["p-weak"]).toMatch(/threshold/);
    expect(applies(dir, base, trunk.diff)).toBe(true);
  });
});
