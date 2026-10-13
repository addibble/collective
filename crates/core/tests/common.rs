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

    /// Like [`Repo::diff`] for raw bytes, as `git diff --binary` writes it.
    pub fn diff_bytes(&self, edits: &[(&str, Option<&[u8]>)]) -> String {
        git(&self.dir, &["reset", "-q", "--hard", &self.base]);
        // `applies` leaves created files behind, untracked.
        git(&self.dir, &["clean", "-fdq"]);
        for (p, c) in edits {
            match c {
                Some(c) => {
                    let path = self.dir.join(p);
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(path, c).unwrap();
                }
                None => {
                    git(&self.dir, &["rm", "-q", p]);
                }
            }
        }
        git(&self.dir, &["add", "-A"]);
        let d = git(&self.dir, &["diff", "--cached", "--binary", &self.base]);
        git(&self.dir, &["reset", "-q", "--hard", &self.base]);
        d
    }

    /// Whether `git apply` accepts the diff on a copy of the base tree that
    /// is not a git repository, which is how the sandbox applies patches.
    /// Returns the patched copy's bytes of `path` when it does.
    pub fn applies_outside_git(&self, diff: &str, path: &str) -> Option<Vec<u8>> {
        git(&self.dir, &["reset", "-q", "--hard", &self.base]);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let tree = self.dir.with_extension(format!("{n}.tree"));
        let _ = fs::remove_dir_all(&tree);
        fs::create_dir_all(&tree).unwrap();
        let tar = Command::new("git")
            .args(["archive", "--format=tar", "-o"])
            .arg(tree.join("base.tar"))
            .arg(&self.base)
            .current_dir(&self.dir)
            .status()
            .unwrap();
        assert!(tar.success());
        assert!(Command::new("tar")
            .args(["-xf", "base.tar"])
            .current_dir(&tree)
            .status()
            .unwrap()
            .success());
        fs::write(tree.join("patch.diff"), diff).unwrap();
        let ok = Command::new("git")
            .args(["apply", "patch.diff"])
            .current_dir(&tree)
            .env("GIT_CEILING_DIRECTORIES", tree.parent().unwrap())
            .status()
            .unwrap()
            .success();
        let out = ok.then(|| fs::read(tree.join(path)).unwrap_or_default());
        let _ = fs::remove_dir_all(&tree);
        out
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
