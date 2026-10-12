// Helpers that build a real git repository so diffs are checked by git itself.
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

export function git(cwd: string, ...args: string[]) {
  return execFileSync("git", args, { cwd, encoding: "utf8", input: "" });
}

export function repo(files: Record<string, string>) {
  const dir = mkdtempSync(join(tmpdir(), "collective-"));
  git(dir, "init", "-q", "-b", "main");
  git(dir, "config", "user.email", "t@example.com");
  git(dir, "config", "user.name", "t");
  for (const [p, c] of Object.entries(files)) {
    mkdirSync(dirname(join(dir, p)), { recursive: true });
    writeFileSync(join(dir, p), c);
  }
  git(dir, "add", "-A");
  git(dir, "commit", "-qm", "base");
  return { dir, base: git(dir, "rev-parse", "HEAD").trim() };
}

/** Applies `edit` to a fresh checkout of base and returns the resulting diff. */
export function makeDiff(dir: string, base: string, edit: (write: (p: string, c: string) => void) => void) {
  git(dir, "checkout", "-q", "--detach", base);
  git(dir, "reset", "-q", "--hard", base);
  edit((p, c) => {
    mkdirSync(dirname(join(dir, p)), { recursive: true });
    writeFileSync(join(dir, p), c);
  });
  git(dir, "add", "-A");
  const diff = git(dir, "diff", "--cached", base);
  git(dir, "reset", "-q", "--hard", base);
  return diff;
}

export function applies(dir: string, base: string, diff: string) {
  git(dir, "reset", "-q", "--hard", base);
  const file = join(dir, "..", `${Math.random().toString(36).slice(2)}.diff`);
  writeFileSync(file, diff);
  try {
    git(dir, "apply", "--check", file);
    git(dir, "apply", file);
    return true;
  } catch {
    return false;
  }
}
