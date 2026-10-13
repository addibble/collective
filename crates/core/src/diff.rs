//! Unified diff (git flavour) parsing, fingerprinting and combination.
//!
//! Supported: modifications, file creation and deletion, of text files and
//! of binary files (`git diff --binary`). Renames, copies and mode-only
//! changes are rejected for now; they make combination semantics much
//! harder and are rare in the patches agents trade.
//!
//! A binary change is carried verbatim and never decoded here. It must name
//! the full object ids of both sides, which `git apply` checks against the
//! file it patches and the file it produces, so a payload that doesn't
//! match what the patch claims fails to apply.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::hash::sha256_hex;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Text after the second `@@`, usually a function signature.
    pub section: String,
    /// Body lines, each starting with ' ', '+', '-' or '\'.
    pub lines: Vec<String>,
}

/// One side of a `GIT binary patch`: its `literal N` or `delta N` header
/// followed by the base85 lines, exactly as git wrote them.
pub type BinaryChunk = Vec<String>;

/// A whole-file binary change (see the module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BinaryChange {
    /// Full object id of the file before; all zeros when it is created.
    pub old_id: String,
    /// Full object id of the file after; all zeros when it is deleted.
    pub new_id: String,
    /// The file mode from the `index` line, when git printed one.
    pub mode: Option<String>,
    pub forward: BinaryChunk,
    /// What `git apply -R` uses. Git always writes it; it is optional here.
    pub reverse: Option<BinaryChunk>,
}

impl BinaryChange {
    /// Lines of encoded payload (at most 52 bytes of compressed data each):
    /// what a binary change counts for where text counts changed lines.
    pub fn payload_lines(&self) -> u32 {
        self.forward.len().saturating_sub(1) as u32
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileDiff {
    /// `None` when the file is created.
    pub old_path: Option<String>,
    /// `None` when the file is deleted.
    pub new_path: Option<String>,
    pub new_mode: Option<String>,
    pub deleted_mode: Option<String>,
    pub hunks: Vec<Hunk>,
    /// Set for a binary file, which has no hunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<BinaryChange>,
}

impl FileDiff {
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffError(pub String);

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DiffError {}

fn err<T>(msg: impl Into<String>) -> Result<T, DiffError> {
    Err(DiffError(msg.into()))
}

fn strip_prefix(p: &str) -> Option<String> {
    if p == "/dev/null" {
        return None;
    }
    // Tolerate a trailing tab + timestamp from non-git diff tools.
    let clean = p.split('\t').next().unwrap_or(p);
    Some(
        clean
            .strip_prefix("a/")
            .or_else(|| clean.strip_prefix("b/"))
            .unwrap_or(clean)
            .to_string(),
    )
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// Parses `@@ -a,b +c,d @@ section`.
fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, rest) = rest.split_once(" @@")?;
    let (old_start, old_lines) = parse_range(old)?;
    let (new_start, new_lines) = parse_range(new)?;
    Some(Hunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        section: rest.strip_prefix(' ').unwrap_or(rest).to_string(),
        lines: Vec::new(),
    })
}

fn new_file() -> FileDiff {
    FileDiff {
        old_path: None,
        new_path: None,
        new_mode: None,
        deleted_mode: None,
        hunks: Vec::new(),
        binary: None,
    }
}

/// The path in `diff --git a/P b/P`. Binary changes have no `---`/`+++`
/// lines, so this header is the only place their path appears. `None` if
/// the two sides differ or the path is quoted.
fn git_header_path(rest: &str) -> Option<&str> {
    let n = rest.len().checked_sub(5)? / 2;
    let (a, b) = (rest.get(..n + 2)?, rest.get(n + 3..)?);
    let path = a.strip_prefix("a/")?;
    (rest.len() == 2 * n + 5 && b.strip_prefix("b/") == Some(path) && !path.is_empty())
        .then_some(path)
}

fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parses `index <old>..<new>[ <mode>]`.
fn parse_index(rest: &str) -> Option<(String, String, Option<String>)> {
    let (ids, mode) = match rest.split_once(' ') {
        Some((ids, mode)) => (ids, Some(mode.to_string())),
        None => (rest, None),
    };
    let (old, new) = ids.split_once("..")?;
    Some((old.to_lowercase(), new.to_lowercase(), mode))
}

/// Git's base85 alphabet, plus the length letter that starts each line.
fn is_base85_line(line: &str) -> bool {
    let b = line.as_bytes();
    b.len() > 1
        && b[0].is_ascii_alphabetic()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || b"!#$%&()*+-;<=>?@^_`{|}~".contains(c))
}

/// Reads one chunk of a binary patch starting at `lines[*i]`, through the
/// blank line that ends it.
fn parse_binary_chunk(lines: &[&str], i: &mut usize) -> Option<BinaryChunk> {
    let header = *lines.get(*i)?;
    let size = header
        .strip_prefix("literal ")
        .or_else(|| header.strip_prefix("delta "))?;
    size.parse::<u64>().ok()?;
    let mut chunk = vec![header.to_string()];
    *i += 1;
    while let Some(l) = lines.get(*i).filter(|l| is_base85_line(l)) {
        chunk.push(l.to_string());
        *i += 1;
    }
    if lines.get(*i).is_some_and(|l| l.is_empty()) {
        *i += 1;
    }
    Some(chunk)
}

pub fn parse_diff(text: &str) -> Result<Vec<FileDiff>, DiffError> {
    let normalized = text.replace("\r\n", "\n");
    let mut lines: Vec<&str> = normalized.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let mut files: Vec<FileDiff> = Vec::new();
    // For the file being read: its `diff --git` path and `index` line.
    let mut header_path: Option<&str> = None;
    let mut index: Option<(String, String, Option<String>)> = None;
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        if let Some(rest) = line.strip_prefix("diff --git ") {
            files.push(new_file());
            header_path = git_header_path(rest);
            index = None;
            i += 1;
            continue;
        }
        if let Some(old) = line.strip_prefix("--- ") {
            let Some(new) = lines.get(i + 1).and_then(|n| n.strip_prefix("+++ ")) else {
                return err(format!("line {}: '---' without '+++'", i + 1));
            };
            // Plain `diff -u` output has no `diff --git` header.
            if files
                .last()
                .is_none_or(|f| !f.hunks.is_empty() || f.binary.is_some())
            {
                files.push(new_file());
            }
            let cur = files.last_mut().expect("just pushed");
            cur.old_path = strip_prefix(old);
            cur.new_path = strip_prefix(new);
            i += 2;
            continue;
        }
        if line.starts_with("@@") {
            let Some(cur) = files.last_mut() else {
                return err(format!("line {}: hunk outside of a file", i + 1));
            };
            let Some(mut hunk) = parse_hunk_header(line) else {
                return err(format!("line {}: malformed hunk header", i + 1));
            };
            i += 1;
            let (mut old_seen, mut new_seen) = (0u32, 0u32);
            while i < lines.len()
                && (old_seen < hunk.old_lines
                    || new_seen < hunk.new_lines
                    || lines[i].starts_with('\\'))
            {
                let body = lines[i];
                match body.as_bytes().first() {
                    None | Some(b' ') => {
                        old_seen += 1;
                        new_seen += 1;
                        hunk.lines.push(if body.is_empty() {
                            " ".into()
                        } else {
                            body.into()
                        });
                    }
                    Some(b'-') => {
                        old_seen += 1;
                        hunk.lines.push(body.into());
                    }
                    Some(b'+') => {
                        new_seen += 1;
                        hunk.lines.push(body.into());
                    }
                    Some(b'\\') => hunk.lines.push(body.into()),
                    _ => return err(format!("line {}: unexpected line inside hunk", i + 1)),
                }
                i += 1;
            }
            if old_seen != hunk.old_lines || new_seen != hunk.new_lines {
                return err(format!("hunk at {} has wrong line counts", hunk.old_start));
            }
            cur.hunks.push(hunk);
            continue;
        }
        if let Some(cur) = files
            .last_mut()
            .filter(|f| f.hunks.is_empty() && f.binary.is_none())
        {
            if let Some(m) = line.strip_prefix("new file mode ") {
                cur.new_mode = Some(m.into());
            } else if let Some(m) = line.strip_prefix("deleted file mode ") {
                cur.deleted_mode = Some(m.into());
            } else if [
                "rename from ",
                "rename to ",
                "copy from ",
                "copy to ",
                "similarity index ",
                "old mode ",
                "new mode ",
            ]
            .iter()
            .any(|p| line.starts_with(p))
            {
                return err(format!("unsupported change: {line}"));
            } else if let Some(rest) = line.strip_prefix("index ") {
                index = parse_index(rest);
            } else if line.starts_with("Binary files ") {
                return err(
                    "binary change without its content: make the diff with `git diff --binary`",
                );
            } else if line == "GIT binary patch" {
                let Some(path) = header_path else {
                    return err(format!(
                        "line {}: binary change without a plain `diff --git a/PATH b/PATH` header",
                        i + 1
                    ));
                };
                let Some((old_id, new_id, mode)) = index.take() else {
                    return err(format!("binary change to {path} needs its `index` line"));
                };
                if !is_object_id(&old_id) || !is_object_id(&new_id) {
                    return err(format!("binary change to {path} needs full object ids: make the diff with `git diff --binary`"));
                }
                i += 1;
                let Some(forward) = parse_binary_chunk(&lines, &mut i) else {
                    return err(format!("malformed binary patch for {path}"));
                };
                let reverse = parse_binary_chunk(&lines, &mut i);
                cur.old_path = cur.new_mode.is_none().then(|| path.to_string());
                cur.new_path = cur.deleted_mode.is_none().then(|| path.to_string());
                cur.binary = Some(BinaryChange {
                    old_id,
                    new_id,
                    mode,
                    forward,
                    reverse,
                });
                continue;
            }
            // `index` lines and other headers are dropped: git recomputes them.
        }
        i += 1;
    }

    if files.is_empty() {
        return err("no file changes found");
    }
    for f in &files {
        if f.old_path.is_none() && f.new_path.is_none() {
            return err("file diff without paths");
        }
        if f.hunks.is_empty() && f.binary.is_none() {
            return err(format!("no hunks for {}", f.path()));
        }
    }
    for (n, f) in files.iter().enumerate() {
        if files[..n].iter().any(|g| g.path() == f.path()) {
            return err(format!("{} appears twice", f.path()));
        }
    }
    Ok(files)
}

pub fn format_diff(files: &[FileDiff]) -> String {
    let mut out = String::new();
    let range = |start: u32, len: u32| {
        if len == 1 {
            start.to_string()
        } else {
            format!("{start},{len}")
        }
    };
    for f in files {
        let a = f
            .old_path
            .as_deref()
            .or(f.new_path.as_deref())
            .unwrap_or_default();
        let b = f
            .new_path
            .as_deref()
            .or(f.old_path.as_deref())
            .unwrap_or_default();
        out.push_str(&format!("diff --git a/{a} b/{b}\n"));
        if f.old_path.is_none() {
            out.push_str(&format!(
                "new file mode {}\n",
                f.new_mode.as_deref().unwrap_or("100644")
            ));
        }
        if f.new_path.is_none() {
            out.push_str(&format!(
                "deleted file mode {}\n",
                f.deleted_mode.as_deref().unwrap_or("100644")
            ));
        }
        if let Some(bin) = &f.binary {
            let mode = bin
                .mode
                .as_ref()
                .map(|m| format!(" {m}"))
                .unwrap_or_default();
            out.push_str(&format!("index {}..{}{mode}\n", bin.old_id, bin.new_id));
            out.push_str("GIT binary patch\n");
            for chunk in std::iter::once(&bin.forward).chain(&bin.reverse) {
                for l in chunk {
                    out.push_str(l);
                    out.push('\n');
                }
                out.push('\n');
            }
            continue;
        }
        match &f.old_path {
            Some(p) => out.push_str(&format!("--- a/{p}\n")),
            None => out.push_str("--- /dev/null\n"),
        }
        match &f.new_path {
            Some(p) => out.push_str(&format!("+++ b/{p}\n")),
            None => out.push_str("+++ /dev/null\n"),
        }
        for h in &f.hunks {
            let section = if h.section.is_empty() {
                String::new()
            } else {
                format!(" {}", h.section)
            };
            out.push_str(&format!(
                "@@ -{} +{} @@{section}\n",
                range(h.old_start, h.old_lines),
                range(h.new_start, h.new_lines)
            ));
            for l in &h.lines {
                out.push_str(l);
                out.push('\n');
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiffStats {
    pub files: Vec<String>,
    pub added: u32,
    pub removed: u32,
    /// Binary files changed; their content is not in `added` or `removed`.
    pub binary: u32,
}

pub fn stats(files: &[FileDiff]) -> DiffStats {
    let (mut added, mut removed) = (0, 0);
    for l in files.iter().flat_map(|f| &f.hunks).flat_map(|h| &h.lines) {
        match l.as_bytes().first() {
            Some(b'+') => added += 1,
            Some(b'-') => removed += 1,
            _ => {}
        }
    }
    DiffStats {
        files: files.iter().map(|f| f.path().to_string()).collect(),
        added,
        removed,
        binary: files.iter().filter(|f| f.binary.is_some()).count() as u32,
    }
}

/// A changed line with whitespace collapsed, tagged with its +/- sign;
/// `None` for context lines and blank changes.
pub fn normalized_change(line: &str) -> Option<String> {
    let tag = line.chars().next()?;
    if tag != '+' && tag != '-' {
        return None;
    }
    let body = line[1..].split_whitespace().collect::<Vec<_>>().join(" ");
    (!body.is_empty()).then(|| format!("{tag}{body}"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HunkFingerprint {
    pub path: String,
    pub fingerprint: String,
}

/// One fingerprint per hunk: the path plus the changed lines with whitespace
/// collapsed. Line numbers and context are ignored, so the same edit made
/// against slightly different bases, or re-indented, still matches. A
/// binary file has one fingerprint: its path and the content it ends up with.
pub fn hunk_fingerprints(files: &[FileDiff]) -> Vec<HunkFingerprint> {
    let mut out = Vec::new();
    for f in files {
        if let Some(bin) = &f.binary {
            let fp = sha256_hex(format!("{}\nbinary {}", f.path(), bin.new_id));
            out.push(HunkFingerprint {
                path: f.path().to_string(),
                fingerprint: fp[..32].to_string(),
            });
        }
        for h in &f.hunks {
            let changed: Vec<String> = h
                .lines
                .iter()
                .filter_map(|l| normalized_change(l))
                .collect();
            if changed.is_empty() {
                continue;
            }
            let fp = sha256_hex(format!("{}\n{}", f.path(), changed.join("\n")));
            out.push(HunkFingerprint {
                path: f.path().to_string(),
                fingerprint: fp[..32].to_string(),
            });
        }
    }
    out
}

/// Content address of a patch: the same diff on the same base is the same patch.
pub fn patch_id(base_commit: &str, files: &[FileDiff]) -> String {
    sha256_hex(format!("{base_commit}\n{}", format_diff(files)))[..24].to_string()
}

// ---------------------------------------------------------------------------
// Combination
// ---------------------------------------------------------------------------

/// Lines of the base file a hunk reads, as a half-open interval in half-lines.
/// A zero-length old range "-k,0" means "insert after line k" and becomes
/// the point between lines k and k+1.
fn old_span(h: &Hunk) -> (u64, u64) {
    if h.old_lines == 0 {
        let p = 2 * h.old_start as u64 + 1;
        (p, p)
    } else {
        (
            2 * h.old_start as u64,
            2 * (h.old_start + h.old_lines) as u64,
        )
    }
}

fn spans_overlap(a: &Hunk, b: &Hunk) -> bool {
    let (a0, a1) = old_span(a);
    let (b0, b1) = old_span(b);
    match (a0 == a1, b0 == b1) {
        (true, true) => a0 == b0,
        // An insertion point conflicts with a span it touches or falls inside.
        (true, false) => a0 + 1 >= b0 && a0 < b1,
        (false, true) => b0 + 1 >= a0 && b0 < a1,
        (false, false) => a0 < b1 && b0 < a1,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Conflict {
    pub path: String,
    pub reason: String,
}

/// Why `next` cannot be layered onto `accepted`, or `None` if both apply
/// cleanly to the same base without touching the same lines.
pub fn find_conflict(accepted: &[FileDiff], next: &[FileDiff]) -> Option<Conflict> {
    for f in next {
        let Some(prev) = accepted.iter().find(|p| p.path() == f.path()) else {
            continue;
        };
        let path = f.path().to_string();
        if prev.binary.is_some() || f.binary.is_some() {
            return Some(Conflict {
                path,
                reason: "both change a binary file".into(),
            });
        }
        if prev.old_path.is_none() || f.old_path.is_none() {
            return Some(Conflict {
                path,
                reason: "both create the file".into(),
            });
        }
        if prev.new_path.is_none() || f.new_path.is_none() {
            return Some(Conflict {
                path,
                reason: "file deleted by one side".into(),
            });
        }
        for h in &f.hunks {
            if prev.hunks.iter().any(|g| spans_overlap(h, g)) {
                return Some(Conflict {
                    path,
                    reason: format!("overlapping edits near line {}", h.old_start),
                });
            }
        }
    }
    None
}

/// Merges non-conflicting diffs against the same base into a single diff.
/// Callers must have checked [`find_conflict`] for each addition.
pub fn combine(diffs: &[&[FileDiff]]) -> Vec<FileDiff> {
    let mut merged: BTreeMap<String, FileDiff> = BTreeMap::new();
    for files in diffs {
        for f in files.iter() {
            match merged.get_mut(f.path()) {
                Some(prev) => prev.hunks.extend(f.hunks.iter().cloned()),
                None => {
                    merged.insert(f.path().to_string(), f.clone());
                }
            }
        }
    }
    for f in merged.values_mut() {
        f.hunks.sort_by_key(|h| old_span(h).0);
        // Recompute new-side starts from the cumulative line delta.
        let mut delta: i64 = 0;
        let created = f.old_path.is_none();
        for h in &mut f.hunks {
            let eff_old = if h.old_lines == 0 {
                h.old_start as i64 + 1
            } else {
                h.old_start as i64
            };
            let eff_new = eff_old + delta;
            h.new_start = if created {
                u32::from(h.new_lines != 0)
            } else if h.new_lines == 0 {
                (eff_new - 1) as u32
            } else {
                eff_new as u32
            };
            delta += h.new_lines as i64 - h.old_lines as i64;
        }
    }
    merged.into_values().collect()
}
