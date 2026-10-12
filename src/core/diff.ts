// Unified diff (git flavour) parsing, fingerprinting and combination.
//
// Everything here is pure so it runs the same in a Worker and in tests.
// Supported: modifications, file creation and deletion. Renames, copies,
// mode-only and binary changes are rejected for now; they make combination
// semantics much harder and are rare in the patches agents trade.

export interface Hunk {
  oldStart: number;
  oldLines: number;
  newStart: number;
  newLines: number;
  section: string; // text after the second @@, usually a function signature
  lines: string[]; // body lines, each starting with ' ', '+', '-' or '\'
}

export interface FileDiff {
  oldPath: string | null; // null when the file is created
  newPath: string | null; // null when the file is deleted
  newMode?: string;
  deletedMode?: string;
  hunks: Hunk[];
}

export class DiffError extends Error {}

const HUNK_RE = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@ ?(.*)$/;

function stripPrefix(p: string): string | null {
  if (p === "/dev/null") return null;
  // Tolerate trailing tab + timestamp from non-git diff tools.
  const clean = p.split("\t")[0]!;
  return clean.startsWith("a/") || clean.startsWith("b/") ? clean.slice(2) : clean;
}

export function parseDiff(text: string): FileDiff[] {
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  const files: FileDiff[] = [];
  let cur: FileDiff | null = null;
  let i = 0;

  while (i < lines.length) {
    const line = lines[i]!;
    if (line.startsWith("diff --git ")) {
      cur = { oldPath: null, newPath: null, hunks: [] };
      files.push(cur);
      i++;
      continue;
    }
    if (line.startsWith("--- ")) {
      const next = lines[i + 1];
      if (next === undefined || !next.startsWith("+++ ")) throw new DiffError(`line ${i + 1}: '---' without '+++'`);
      // Plain `diff -u` output has no `diff --git` header.
      if (!cur || cur.hunks.length > 0) {
        cur = { oldPath: null, newPath: null, hunks: [] };
        files.push(cur);
      }
      cur.oldPath = stripPrefix(line.slice(4));
      cur.newPath = stripPrefix(next.slice(4));
      i += 2;
      continue;
    }
    if (line.startsWith("@@")) {
      if (!cur) throw new DiffError(`line ${i + 1}: hunk outside of a file`);
      const m = HUNK_RE.exec(line);
      if (!m) throw new DiffError(`line ${i + 1}: malformed hunk header`);
      const hunk: Hunk = {
        oldStart: Number(m[1]),
        oldLines: m[2] === undefined ? 1 : Number(m[2]),
        newStart: Number(m[3]),
        newLines: m[4] === undefined ? 1 : Number(m[4]),
        section: m[5] ?? "",
        lines: [],
      };
      i++;
      let oldSeen = 0;
      let newSeen = 0;
      while (i < lines.length && (oldSeen < hunk.oldLines || newSeen < hunk.newLines || lines[i]!.startsWith("\\"))) {
        const body = lines[i]!;
        const tag = body[0];
        if (tag === " " || body === "") {
          oldSeen++;
          newSeen++;
          hunk.lines.push(body === "" ? " " : body);
        } else if (tag === "-") {
          oldSeen++;
          hunk.lines.push(body);
        } else if (tag === "+") {
          newSeen++;
          hunk.lines.push(body);
        } else if (tag === "\\") {
          hunk.lines.push(body);
        } else {
          throw new DiffError(`line ${i + 1}: unexpected line inside hunk`);
        }
        i++;
      }
      if (oldSeen !== hunk.oldLines || newSeen !== hunk.newLines) {
        throw new DiffError(`hunk at ${hunk.oldStart} has wrong line counts`);
      }
      cur.hunks.push(hunk);
      continue;
    }
    if (cur && cur.hunks.length === 0) {
      if (line.startsWith("new file mode ")) cur.newMode = line.slice(14);
      else if (line.startsWith("deleted file mode ")) cur.deletedMode = line.slice(18);
      else if (/^(rename|copy) (from|to) |^similarity index |^old mode |^new mode /.test(line)) {
        throw new DiffError(`unsupported change: ${line}`);
      } else if (line.startsWith("Binary files ") || line === "GIT binary patch") {
        throw new DiffError("binary patches are not supported");
      }
      // index lines and other headers are dropped: they are recomputed by git.
    }
    i++;
  }

  if (files.length === 0) throw new DiffError("no file changes found");
  for (const f of files) {
    if (f.oldPath === null && f.newPath === null) throw new DiffError("file diff without paths");
    if (f.hunks.length === 0) throw new DiffError(`no hunks for ${f.newPath ?? f.oldPath}`);
  }
  return files;
}

export function filePath(f: FileDiff): string {
  return (f.newPath ?? f.oldPath)!;
}

export function formatDiff(files: FileDiff[]): string {
  const out: string[] = [];
  for (const f of files) {
    const a = f.oldPath ?? f.newPath!;
    const b = f.newPath ?? f.oldPath!;
    out.push(`diff --git a/${a} b/${b}`);
    if (f.oldPath === null) out.push(`new file mode ${f.newMode ?? "100644"}`);
    if (f.newPath === null) out.push(`deleted file mode ${f.deletedMode ?? "100644"}`);
    out.push(`--- ${f.oldPath === null ? "/dev/null" : `a/${f.oldPath}`}`);
    out.push(`+++ ${f.newPath === null ? "/dev/null" : `b/${f.newPath}`}`);
    for (const h of f.hunks) {
      const range = (start: number, len: number) => (len === 1 ? `${start}` : `${start},${len}`);
      const section = h.section ? ` ${h.section}` : "";
      out.push(`@@ -${range(h.oldStart, h.oldLines)} +${range(h.newStart, h.newLines)} @@${section}`);
      out.push(...h.lines);
    }
  }
  return out.join("\n") + "\n";
}

export interface DiffStats {
  files: string[];
  added: number;
  removed: number;
}

export function stats(files: FileDiff[]): DiffStats {
  let added = 0;
  let removed = 0;
  for (const f of files)
    for (const h of f.hunks)
      for (const l of h.lines) {
        if (l[0] === "+") added++;
        else if (l[0] === "-") removed++;
      }
  return { files: files.map(filePath), added, removed };
}

export async function sha256Hex(text: string): Promise<string> {
  const buf = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(buf)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * One fingerprint per hunk: the path plus the changed lines with whitespace
 * collapsed. Line numbers and context are ignored, so the same edit made
 * against slightly different bases, or re-indented, still matches.
 */
export async function hunkFingerprints(files: FileDiff[]): Promise<{ path: string; fingerprint: string }[]> {
  const out: { path: string; fingerprint: string }[] = [];
  for (const f of files) {
    const path = filePath(f);
    for (const h of f.hunks) {
      const changed = h.lines
        .filter((l) => l[0] === "+" || l[0] === "-")
        .map((l) => l[0] + l.slice(1).trim().replace(/\s+/g, " "))
        .filter((l) => l.length > 1);
      if (changed.length === 0) continue;
      out.push({ path, fingerprint: (await sha256Hex(`${path}\n${changed.join("\n")}`)).slice(0, 32) });
    }
  }
  return out;
}

/** Content address of a patch: the same diff on the same base is the same patch. */
export async function patchId(baseCommit: string, files: FileDiff[]): Promise<string> {
  return (await sha256Hex(`${baseCommit}\n${formatDiff(files)}`)).slice(0, 24);
}

// ---------------------------------------------------------------------------
// Combination
// ---------------------------------------------------------------------------

/** Lines of the base file a hunk reads, as a half-open interval. */
function oldSpan(h: Hunk): [number, number] {
  // A zero-length old range "-k,0" means "insert after line k".
  if (h.oldLines === 0) return [h.oldStart + 0.5, h.oldStart + 0.5];
  return [h.oldStart, h.oldStart + h.oldLines];
}

function spansOverlap(a: Hunk, b: Hunk): boolean {
  const [a0, a1] = oldSpan(a);
  const [b0, b1] = oldSpan(b);
  if (a0 === a1 || b0 === b1) {
    // Insertion points conflict with anything that touches them or each other.
    const p = a0 === a1 ? a0 : b0;
    const [s0, s1] = a0 === a1 ? [b0, b1] : [a0, a1];
    if (s0 === s1) return s0 === p;
    return p >= s0 - 0.5 && p <= s1 - 0.5;
  }
  return a0 < b1 && b0 < a1;
}

export type Conflict = { path: string; reason: string };

/**
 * Returns why `next` cannot be layered onto `accepted`, or null if both
 * apply cleanly to the same base without touching the same lines.
 */
export function findConflict(accepted: FileDiff[], next: FileDiff[]): Conflict | null {
  const byPath = new Map(accepted.map((f) => [filePath(f), f]));
  for (const f of next) {
    const path = filePath(f);
    const prev = byPath.get(path);
    if (!prev) continue;
    if (prev.oldPath === null || f.oldPath === null) return { path, reason: "both create the file" };
    if (prev.newPath === null || f.newPath === null) return { path, reason: "file deleted by one side" };
    for (const h of f.hunks)
      for (const g of prev.hunks)
        if (spansOverlap(h, g)) return { path, reason: `overlapping edits near line ${h.oldStart}` };
  }
  return null;
}

/**
 * Merges non-conflicting diffs against the same base into a single diff.
 * Caller must have checked findConflict for each addition.
 */
export function combine(diffs: FileDiff[][]): FileDiff[] {
  const merged = new Map<string, FileDiff>();
  for (const files of diffs) {
    for (const f of files) {
      const path = filePath(f);
      const prev = merged.get(path);
      if (!prev) merged.set(path, { ...f, hunks: f.hunks.map((h) => ({ ...h, lines: [...h.lines] })) });
      else prev.hunks.push(...f.hunks.map((h) => ({ ...h, lines: [...h.lines] })));
    }
  }
  for (const f of merged.values()) {
    f.hunks.sort((a, b) => oldSpan(a)[0] - oldSpan(b)[0]);
    // Recompute new-side starts from the cumulative line delta.
    let delta = 0;
    for (const h of f.hunks) {
      const effOld = h.oldLines === 0 ? h.oldStart + 1 : h.oldStart;
      const effNew = effOld + delta;
      h.newStart = h.newLines === 0 ? effNew - 1 : effNew;
      if (f.oldPath === null) h.newStart = h.newLines === 0 ? 0 : 1;
      delta += h.newLines - h.oldLines;
    }
  }
  return [...merged.values()].sort((a, b) => filePath(a).localeCompare(filePath(b)));
}
