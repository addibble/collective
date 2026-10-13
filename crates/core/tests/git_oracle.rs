//! Checks collective_core::git against real git: trees we build match
//! `git write-tree`, and our pushes and fetches go through `git receive-pack`
//! and `git upload-pack` (the same programs behind smart HTTP). The library
//! stays pure; git is only the oracle here. Skipped when git is missing.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use collective_core::diff::parse_diff;
use collective_core::git::*;

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("collective-git-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn git(dir: &Path, args: &[&str]) -> String {
    git_io(dir, args, None, &[])
}

fn git_io(dir: &Path, args: &[&str], input: Option<&[u8]>, env: &[(&str, &str)]) -> String {
    String::from_utf8(git_bytes(dir, args, input, env)).unwrap()
}

fn git_bytes(dir: &Path, args: &[&str], input: Option<&[u8]>, env: &[(&str, &str)]) -> Vec<u8> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    if let Some(i) = input {
        stdin.write_all(i).unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn write(dir: &Path, path: &str, content: &str) {
    let p = dir.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn ls_tree(dir: &Path, treeish: &str) -> Vec<TreeEntry> {
    git(dir, &["ls-tree", treeish])
        .lines()
        .map(|l| {
            let (meta, name) = l.split_once('\t').unwrap();
            let mut parts = meta.split(' ');
            let mode = parts.next().unwrap();
            let _kind = parts.next();
            TreeEntry {
                // ls-tree pads tree modes to 040000; trees store 40000.
                mode: mode.trim_start_matches('0').to_string(),
                name: name.to_string(),
                id: parts.next().unwrap().to_string(),
            }
        })
        .collect()
}

fn push(bare: &Path, updates: &[RefUpdate], pack: Option<&[u8]>) -> Vec<(String, Option<String>)> {
    let req = receive_pack_request(updates, pack).unwrap();
    let resp = git_bytes(
        bare,
        &["receive-pack", "--stateless-rpc", "."],
        Some(&req),
        &[],
    );
    parse_report_status(&resp).unwrap()
}

#[test]
fn patch_commits_match_git_and_push_through_receive_pack() {
    if !git_available() {
        eprintln!("git not found; skipping");
        return;
    }
    let up = tmp("up");
    git(&up, &["init", "-q", "-b", "main"]);
    write(&up, "README", "hello\n");
    write(&up, "src/lib.rs", "fn a() {}\n\nfn b() {}\n\nfn c() {}\n");
    write(&up, "src/old.rs", "gone\n");
    write(&up, "docs/a.md", "no newline");
    write(&up, "src/nested/keep.txt", "keep\n");
    git(&up, &["add", "-A"]);
    git(&up, &["commit", "-q", "-m", "base"]);
    let base = git(&up, &["rev-parse", "HEAD"]).trim().to_string();

    write(
        &up,
        "src/lib.rs",
        "fn a() {}\n\nfn b() { todo!() }\n\nfn c() {}\nfn d() {}\n",
    );
    std::fs::remove_file(up.join("src/old.rs")).unwrap();
    write(&up, "new/deep/file.txt", "fresh\n");
    write(&up, "docs/a.md", "now with newline\n");
    git(&up, &["add", "-A"]);
    let diff = git(&up, &["diff", "--cached", "--no-renames"]);
    let expected_tree = git(&up, &["write-tree"]).trim().to_string();

    let files = parse_diff(&diff).unwrap();
    let mut changes = BTreeMap::new();
    for f in &files {
        let old = f
            .old_path
            .as_ref()
            .map(|p| git_bytes(&up, &["show", &format!("{base}:{p}")], None, &[]));
        let change = match apply(old.as_deref(), f).unwrap() {
            Some(content) => Change::Write {
                mode: FILE_MODE.into(),
                content,
            },
            None => Change::Delete,
        };
        changes.insert(f.path().to_string(), change);
    }
    let mut trees = BTreeMap::new();
    for dir in dirs_to_read(changes.keys().map(String::as_str)) {
        let spec = format!("{base}:{dir}");
        let exists = Command::new("git")
            .current_dir(&up)
            .args(["cat-file", "-e", &spec])
            .status()
            .unwrap()
            .success();
        if exists {
            trees.insert(dir.clone(), ls_tree(&up, &spec));
        }
    }
    let (tree, mut objects) = edit_tree(&trees, &changes).unwrap();
    assert_eq!(tree, expected_tree, "our tree must match git write-tree");

    let commit = patch_commit(
        &base,
        &tree,
        &"f".repeat(64),
        "Do things",
        "alice",
        1_700_000_000,
    )
    .unwrap();
    objects.push(commit.clone());
    let pack = write_pack(&objects);

    let bare = tmp("bare");
    git(&bare, &["clone", "-q", "--bare", up.to_str().unwrap(), "."]);
    let name = patch_ref(&"f".repeat(64));
    let report = push(
        &bare,
        &[RefUpdate {
            name: name.clone(),
            old: ZERO_ID.into(),
            new: commit.id.clone(),
        }],
        Some(&pack),
    );
    assert_eq!(report, vec![(name.clone(), None)]);
    assert_eq!(git(&bare, &["rev-parse", &name]).trim(), commit.id);
    git(&bare, &["fsck", "--no-dangling"]);

    // The advertisement now lists the branch, and deleting it needs no pack.
    let adv = git_bytes(
        &bare,
        &["receive-pack", "--stateless-rpc", "--advertise-refs", "."],
        None,
        &[],
    );
    let refs = parse_advertisement(&adv).unwrap();
    assert_eq!(refs[&name], commit.id);
    let report = push(
        &bare,
        &[RefUpdate {
            name: name.clone(),
            old: commit.id.clone(),
            new: ZERO_ID.into(),
        }],
        None,
    );
    assert_eq!(report, vec![(name, None)]);

    let _ = std::fs::remove_dir_all(&up);
    let _ = std::fs::remove_dir_all(&bare);
}

#[test]
fn v2_fetch_packs_can_be_pushed_onward() {
    if !git_available() {
        eprintln!("git not found; skipping");
        return;
    }
    let up = tmp("fetch-up");
    git(&up, &["init", "-q", "-b", "main"]);
    write(&up, "a", "1\n");
    git(&up, &["add", "-A"]);
    git(&up, &["commit", "-q", "-m", "one"]);
    let first = git(&up, &["rev-parse", "HEAD"]).trim().to_string();

    let fetch = |wants: &[String], haves: &[String]| {
        let req = fetch_request(wants, haves).unwrap();
        let resp = git_bytes(
            &up,
            &["upload-pack", "--stateless-rpc", "."],
            Some(&req),
            &[("GIT_PROTOCOL", "version=2")],
        );
        fetch_response_pack(&resp).unwrap()
    };

    let mirror = tmp("fetch-mirror");
    git(&mirror, &["init", "-q", "--bare"]);
    let pack = fetch(std::slice::from_ref(&first), &[]);
    let r = push(
        &mirror,
        &[RefUpdate {
            name: base_ref(&first),
            old: ZERO_ID.into(),
            new: first.clone(),
        }],
        Some(&pack),
    );
    assert_eq!(r[0].1, None);

    // A later base: fetch only what the mirror lacks, then push it.
    write(&up, "b", "2\n");
    git(&up, &["add", "-A"]);
    git(&up, &["commit", "-q", "-m", "two"]);
    let second = git(&up, &["rev-parse", "HEAD"]).trim().to_string();
    let delta = fetch(std::slice::from_ref(&second), std::slice::from_ref(&first));
    assert!(delta.len() < pack.len() + 200);
    let r = push(
        &mirror,
        &[RefUpdate {
            name: base_ref(&second),
            old: ZERO_ID.into(),
            new: second.clone(),
        }],
        Some(&delta),
    );
    assert_eq!(r[0].1, None);
    git(&mirror, &["fsck", "--no-dangling"]);
    assert_eq!(
        git(&mirror, &["rev-parse", &base_ref(&second)]).trim(),
        second
    );

    let _ = std::fs::remove_dir_all(&up);
    let _ = std::fs::remove_dir_all(&mirror);
}
