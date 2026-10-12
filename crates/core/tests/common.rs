// Helpers that build a real git repository so diffs are checked by git itself.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

pub struct Repo {
    pub dir: PathBuf,
    pub base: String,
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn write(dir: &Path, path: &str, content: &str) {
    let p = dir.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

pub fn lines(n: usize, tag: &str) -> String {
    (1..=n).map(|i| format!("{tag} {i}\n")).collect()
}

impl Repo {
    pub fn new(files: &[(&str, &str)]) -> Repo {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("collective-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "t"]);
        for (p, c) in files {
            write(&dir, p, c);
        }
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "base"]);
        let base = git(&dir, &["rev-parse", "HEAD"]).trim().to_string();
        Repo { dir, base }
    }

    pub fn read(&self, path: &str) -> String {
        fs::read_to_string(self.dir.join(path)).unwrap()
    }

    /// Applies `edits` (path, new content or None to delete) to base and returns the diff.
    pub fn diff(&self, edits: &[(&str, Option<String>)]) -> String {
        git(&self.dir, &["reset", "-q", "--hard", &self.base]);
        for (p, c) in edits {
            match c {
                Some(c) => write(&self.dir, p, c),
                None => {
                    git(&self.dir, &["rm", "-q", p]);
                }
            }
        }
        git(&self.dir, &["add", "-A"]);
        let d = git(&self.dir, &["diff", "--cached", &self.base]);
        git(&self.dir, &["reset", "-q", "--hard", &self.base]);
        d
    }

    /// Replaces line `at` (1-based) of `path` and returns the diff.
    pub fn edit_line(&self, path: &str, at: usize, text: &str) -> String {
        git(&self.dir, &["reset", "-q", "--hard", &self.base]);
        let mut l: Vec<String> = self.read(path).lines().map(String::from).collect();
        l[at - 1] = text.into();
        self.diff(&[(path, Some(l.join("\n") + "\n"))])
    }

    /// Whether `git apply` accepts the diff on a clean base; leaves it applied.
    pub fn applies(&self, diff: &str) -> bool {
        git(&self.dir, &["reset", "-q", "--hard", &self.base]);
        let file = self
            .dir
            .with_extension(format!("{}.diff", COUNTER.fetch_add(1, Ordering::SeqCst)));
        fs::write(&file, diff).unwrap();
        let ok = Command::new("git")
            .args(["apply", file.to_str().unwrap()])
            .current_dir(&self.dir)
            .status()
            .unwrap()
            .success();
        let _ = fs::remove_file(file);
        ok
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
