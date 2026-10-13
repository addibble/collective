//! Git hosting for patches and trunks (docs/GIT.md): applying parsed diffs to
//! file contents, encoding blobs, trees and commits, writing packs, and the
//! smart-HTTP wire format the Worker speaks to its Git host (pkt-lines,
//! receive-pack, protocol-v2 fetch).
//!
//! Pure like the rest of the core: the Worker reads trees and blobs from the
//! repository, passes them in with the time, and sends the bytes built here.
//! The same inputs always give the same object ids, so anyone can rebuild a
//! patch's commit from its diff and check the server's branch.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sha1::{Digest, Sha1};

use crate::diff::FileDiff;

#[derive(Debug, Clone, PartialEq)]
pub struct GitError(pub String);

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GitError {}

pub type GitResult<T> = Result<T, GitError>;

fn fail<T>(msg: impl Into<String>) -> GitResult<T> {
    Err(GitError(msg.into()))
}

/// The id git uses for "no object": the old side of a ref being created.
pub const ZERO_ID: &str = "0000000000000000000000000000000000000000";
pub const FILE_MODE: &str = "100644";
pub const TREE_MODE: &str = "40000";

/// A full, lowercase SHA-1 object id.
pub fn is_object_id(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(id: &str) -> GitResult<[u8; 20]> {
    if !is_object_id(id) {
        return fail(format!("not an object id: {id}"));
    }
    let mut out = [0u8; 20];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&id[2 * i..2 * i + 2], 16).expect("checked hex");
    }
    Ok(out)
}

// -- refs ---------------------------------------------------------------------

/// The branch holding a patch: one commit on top of its base.
pub fn patch_ref(patch_id: &str) -> String {
    format!("refs/heads/patch/{patch_id}")
}

/// The branch holding the converged trunk for a base.
pub fn trunk_ref(base: &str) -> String {
    format!("refs/heads/trunk/{base}")
}

/// The branch that keeps an upstream base commit reachable in the repository.
pub fn base_ref(base: &str) -> String {
    format!("refs/heads/base/{base}")
}

/// The repository name for a project. Names allow letters, digits, dots,
/// hyphens and underscores; anything else becomes a hyphen.
pub fn repo_name(project_id: &str) -> String {
    let clean: String = project_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("p-{clean}")
}

fn valid_ref(name: &str) -> bool {
    name.strip_prefix("refs/heads/").is_some_and(|rest| {
        !rest.is_empty()
            && rest.split('/').all(|part| {
                !part.is_empty()
                    && !part.starts_with('.')
                    && !part.ends_with(".lock")
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            })
            && !rest.contains("..")
    })
}

// -- applying diffs -----------------------------------------------------------

/// Applies one file's hunks to its content in the base. `old` is `None` when
/// the file doesn't exist there; the result is `None` when the diff deletes
/// it. Context must match exactly (no fuzz): patches are stored against their
/// base, and a trunk's hunks are disjoint in it. A file whose lines end in
/// CRLF keeps them, including on added lines.
pub fn apply(old: Option<&[u8]>, file: &FileDiff) -> GitResult<Option<Vec<u8>>> {
    let path = file.path();
    match (file.old_path.is_some(), old.is_some()) {
        (true, false) => return fail(format!("{path}: not in the base tree")),
        (false, true) => return fail(format!("{path}: already exists in the base tree")),
        _ => {}
    }
    let text = match old {
        Some(b) => std::str::from_utf8(b)
            .map_err(|_| GitError(format!("{path}: not UTF-8 text in the base")))?,
        None => "",
    };
    let crlf = text.find('\n').is_some_and(|i| text[..i].ends_with('\r'));
    let old_final_eol = text.is_empty() || text.ends_with('\n');
    let mut old_lines: Vec<&str> = text.split('\n').collect();
    if text.is_empty() {
        old_lines.clear();
    } else if old_final_eol {
        old_lines.pop();
    }

    let mismatch = |line: u32| {
        GitError(format!(
            "{path}: hunk at line {line} does not match the base"
        ))
    };
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    // Length of `out` when a "\ No newline at end of file" followed a new-side line.
    let mut no_eol_at: Option<usize> = None;
    for h in &file.hunks {
        let start = if h.old_lines == 0 {
            h.old_start as usize
        } else {
            (h.old_start as usize)
                .checked_sub(1)
                .ok_or_else(|| mismatch(h.old_start))?
        };
        if start < cursor || start > old_lines.len() {
            return Err(mismatch(h.old_start));
        }
        out.extend(old_lines[cursor..start].iter().map(|s| s.to_string()));
        let mut pos = start;
        let mut prev = None;
        for l in &h.lines {
            let Some(&kind) = l.as_bytes().first() else {
                return Err(mismatch(h.old_start));
            };
            let body = &l[1..];
            match kind {
                b' ' | b'-' => {
                    let Some(o) = old_lines.get(pos) else {
                        return Err(mismatch(h.old_start));
                    };
                    if o.strip_suffix('\r').unwrap_or(o) != body {
                        return Err(mismatch(h.old_start));
                    }
                    if kind == b' ' {
                        out.push(o.to_string());
                    }
                    pos += 1;
                }
                b'+' => out.push(if crlf {
                    format!("{body}\r")
                } else {
                    body.to_string()
                }),
                b'\\' => {
                    if matches!(prev, Some(b'+' | b' ')) {
                        no_eol_at = Some(out.len());
                    }
                }
                _ => return Err(mismatch(h.old_start)),
            }
            prev = Some(kind);
        }
        cursor = pos;
    }
    let reached_end = cursor == old_lines.len();
    out.extend(old_lines[cursor..].iter().map(|s| s.to_string()));

    if file.new_path.is_none() {
        if !out.is_empty() {
            return fail(format!("{path}: deletion leaves lines behind"));
        }
        return Ok(None);
    }
    let final_eol = if reached_end && !file.hunks.is_empty() {
        no_eol_at != Some(out.len())
    } else {
        old_final_eol
    };
    let mut content = out.join("\n");
    if final_eol && !out.is_empty() {
        content.push('\n');
    }
    Ok(Some(content.into_bytes()))
}

// -- objects ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Commit = 1,
    Tree = 2,
    Blob = 3,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub kind: Kind,
    pub body: Vec<u8>,
    pub id: String,
}

impl Object {
    pub fn new(kind: Kind, body: Vec<u8>) -> Object {
        let id = object_id(kind, &body);
        Object { kind, body, id }
    }
}

pub fn object_id(kind: Kind, body: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("{} {}\0", kind.name(), body.len()).as_bytes());
    h.update(body);
    hex(&h.finalize())
}

/// One entry of a tree, as the Git host reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TreeEntry {
    pub mode: String,
    pub name: String,
    pub id: String,
}

fn is_tree(mode: &str) -> bool {
    mode == TREE_MODE
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.eq_ignore_ascii_case(".git")
        && !name.contains(['/', '\0', '\n'])
}

/// Encodes a tree, sorting entries the way git does (a directory sorts as if
/// its name ended in `/`).
pub fn encode_tree(entries: &[TreeEntry]) -> GitResult<Vec<u8>> {
    let mut sorted: Vec<&TreeEntry> = entries.iter().collect();
    let key = |e: &TreeEntry| {
        let mut k = e.name.as_bytes().to_vec();
        if is_tree(&e.mode) {
            k.push(b'/');
        }
        k
    };
    sorted.sort_by_key(|e| key(e));
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for e in sorted {
        if !valid_name(&e.name) || !seen.insert(e.name.as_str()) {
            return fail(format!("invalid or duplicate tree entry: {:?}", e.name));
        }
        if !matches!(
            e.mode.as_str(),
            "100644" | "100755" | "120000" | "160000" | "40000"
        ) {
            return fail(format!("unsupported mode {} for {}", e.mode, e.name));
        }
        out.extend_from_slice(e.mode.as_bytes());
        out.push(b' ');
        out.extend_from_slice(e.name.as_bytes());
        out.push(0);
        out.extend_from_slice(&unhex(&e.id)?);
    }
    Ok(out)
}

/// An author or committer. Times are Unix seconds, always written as UTC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: i64,
}

impl Signature {
    fn line(&self) -> String {
        let clean = |s: &str| s.replace(['<', '>', '\n', '\0'], "").trim().to_string();
        format!(
            "{} <{}> {} +0000",
            clean(&self.name),
            clean(&self.email),
            self.time
        )
    }
}

pub fn encode_commit(
    tree: &str,
    parents: &[String],
    author: &Signature,
    committer: &Signature,
    message: &str,
) -> GitResult<Vec<u8>> {
    unhex(tree)?;
    let mut out = format!("tree {tree}\n");
    for p in parents {
        unhex(p)?;
        out.push_str(&format!("parent {p}\n"));
    }
    out.push_str(&format!("author {}\n", author.line()));
    out.push_str(&format!("committer {}\n\n", committer.line()));
    out.push_str(message.replace('\0', "").trim_end());
    out.push('\n');
    Ok(out.into_bytes())
}

// -- editing trees ------------------------------------------------------------

pub enum Change {
    Write { mode: String, content: Vec<u8> },
    Delete,
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn base_name(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, n)| n).unwrap_or(path)
}

fn check_path(path: &str) -> GitResult<()> {
    if path.split('/').all(valid_name) {
        Ok(())
    } else {
        fail(format!("refusing path {path:?}"))
    }
}

/// The directories whose current entries [`edit_tree`] needs to change
/// `paths`, parents before children. `""` is the root.
pub fn dirs_to_read<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut dirs = BTreeSet::new();
    for p in paths {
        let mut d = parent_dir(p);
        loop {
            dirs.insert(d.to_string());
            if d.is_empty() {
                break;
            }
            d = parent_dir(d);
        }
    }
    let mut v: Vec<String> = dirs.into_iter().collect();
    v.sort_by_key(|d| {
        (
            if d.is_empty() {
                0
            } else {
                d.matches('/').count() + 1
            },
            d.clone(),
        )
    });
    v
}

/// Applies `changes` (path → new content or deletion) to a tree. `trees`
/// holds the current entries of every directory from [`dirs_to_read`] that
/// exists, including the root (`""`). Returns the new root tree id and the
/// objects to send. Directories left empty disappear, as in git.
pub fn edit_tree(
    trees: &BTreeMap<String, Vec<TreeEntry>>,
    changes: &BTreeMap<String, Change>,
) -> GitResult<(String, Vec<Object>)> {
    for p in changes.keys() {
        check_path(p)?;
    }
    if !trees.contains_key("") {
        return fail("the root tree was not read");
    }
    let dirs: BTreeSet<String> = dirs_to_read(changes.keys().map(String::as_str))
        .into_iter()
        .collect();
    let mut objects = Vec::new();
    let root = build_dir("", trees, changes, &dirs, &mut objects)?;
    let root = match root {
        Some(id) => id,
        None => {
            let empty = Object::new(Kind::Tree, Vec::new());
            let id = empty.id.clone();
            objects.push(empty);
            id
        }
    };
    Ok((root, objects))
}

fn build_dir(
    dir: &str,
    trees: &BTreeMap<String, Vec<TreeEntry>>,
    changes: &BTreeMap<String, Change>,
    dirs: &BTreeSet<String>,
    objects: &mut Vec<Object>,
) -> GitResult<Option<String>> {
    let mut entries: BTreeMap<String, TreeEntry> = trees
        .get(dir)
        .into_iter()
        .flatten()
        .map(|e| (e.name.clone(), e.clone()))
        .collect();
    for (path, change) in changes.iter().filter(|(p, _)| parent_dir(p) == dir) {
        let name = base_name(path);
        match change {
            Change::Write { mode, content } => {
                if entries.get(name).is_some_and(|e| is_tree(&e.mode)) {
                    return fail(format!("{path} is a directory in the base"));
                }
                let blob = Object::new(Kind::Blob, content.clone());
                entries.insert(
                    name.to_string(),
                    TreeEntry {
                        mode: mode.clone(),
                        name: name.to_string(),
                        id: blob.id.clone(),
                    },
                );
                objects.push(blob);
            }
            Change::Delete => match entries.remove(name) {
                Some(e) if !is_tree(&e.mode) => {}
                _ => return fail(format!("{path}: not a file in the base")),
            },
        }
    }
    for child in dirs
        .iter()
        .filter(|d| !d.is_empty() && parent_dir(d) == dir)
    {
        let name = base_name(child);
        match entries.get(name) {
            Some(e) if !is_tree(&e.mode) => return fail(format!("{child} is a file in the base")),
            Some(_) if !trees.contains_key(child.as_str()) => {
                return fail(format!("the tree for {child} was not read"))
            }
            _ => {}
        }
        match build_dir(child, trees, changes, dirs, objects)? {
            Some(id) => {
                entries.insert(
                    name.to_string(),
                    TreeEntry {
                        mode: TREE_MODE.into(),
                        name: name.to_string(),
                        id,
                    },
                );
            }
            None => {
                entries.remove(name);
            }
        }
    }
    if entries.is_empty() && !dir.is_empty() {
        return Ok(None);
    }
    let list: Vec<TreeEntry> = entries.into_values().collect();
    let tree = Object::new(Kind::Tree, encode_tree(&list)?);
    let id = tree.id.clone();
    objects.push(tree);
    Ok(Some(id))
}

// -- packs --------------------------------------------------------------------

/// Writes a version-2 pack of whole (undeltified) objects, each once.
pub fn write_pack(objects: &[Object]) -> Vec<u8> {
    let mut seen = BTreeSet::new();
    let unique: Vec<&Object> = objects.iter().filter(|o| seen.insert(&o.id)).collect();
    let mut out = b"PACK".to_vec();
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(unique.len() as u32).to_be_bytes());
    for o in unique {
        let mut size = o.body.len();
        let mut byte = ((o.kind as u8) << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            out.push(byte | 0x80);
            byte = (size & 0x7f) as u8;
            size >>= 7;
        }
        out.push(byte);
        out.extend(miniz_oxide::deflate::compress_to_vec_zlib(&o.body, 6));
    }
    let digest = Sha1::digest(&out);
    out.extend_from_slice(&digest);
    out
}

// -- pkt-lines ----------------------------------------------------------------

pub const FLUSH: &[u8] = b"0000";
pub const DELIM: &[u8] = b"0001";

pub fn pkt_line(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", data.len() + 4).into_bytes();
    out.extend_from_slice(data);
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum Pkt<'a> {
    Flush,
    Delim,
    End,
    Data(&'a [u8]),
}

pub fn pkt_lines(bytes: &[u8]) -> GitResult<Vec<Pkt<'_>>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let Some(head) = bytes.get(pos..pos + 4) else {
            return fail("truncated pkt-line");
        };
        let len = std::str::from_utf8(head)
            .ok()
            .and_then(|h| usize::from_str_radix(h, 16).ok())
            .ok_or_else(|| GitError("malformed pkt-line length".into()))?;
        match len {
            0 => out.push(Pkt::Flush),
            1 => out.push(Pkt::Delim),
            2 => out.push(Pkt::End),
            3 => return fail("malformed pkt-line length"),
            n => {
                let Some(data) = bytes.get(pos + 4..pos + n) else {
                    return fail("truncated pkt-line");
                };
                out.push(Pkt::Data(data));
                pos += n;
                continue;
            }
        }
        pos += 4;
    }
    Ok(out)
}

fn text_line(d: &[u8]) -> String {
    String::from_utf8_lossy(d)
        .trim_end_matches('\n')
        .to_string()
}

fn err_line(d: &[u8]) -> Option<String> {
    text_line(d).strip_prefix("ERR ").map(str::to_string)
}

// -- receive-pack (push) ------------------------------------------------------

/// The refs a server advertises (`GET info/refs?service=…`), as name → id.
pub fn parse_advertisement(bytes: &[u8]) -> GitResult<BTreeMap<String, String>> {
    let mut refs = BTreeMap::new();
    let mut pkts = pkt_lines(bytes)?.into_iter().peekable();
    if let Some(Pkt::Data(d)) = pkts.peek() {
        if d.starts_with(b"# service=") {
            pkts.next();
            if pkts.next() != Some(Pkt::Flush) {
                return fail("malformed ref advertisement");
            }
        }
    }
    for p in pkts {
        let Pkt::Data(d) = p else { break };
        if let Some(e) = err_line(d) {
            return fail(format!("server error: {e}"));
        }
        let line = text_line(d);
        let line = line.split('\0').next().unwrap_or_default();
        let Some((id, name)) = line.split_once(' ') else {
            return fail("malformed ref advertisement");
        };
        if name == "capabilities^{}" || name.ends_with("^{}") {
            continue;
        }
        if !is_object_id(id) {
            return fail("malformed ref advertisement");
        }
        refs.insert(name.to_string(), id.to_string());
    }
    Ok(refs)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    pub name: String,
    pub old: String,
    pub new: String,
}

/// The body of a `git-receive-pack` POST: ref updates, then the pack
/// (required unless every update is a deletion).
pub fn receive_pack_request(updates: &[RefUpdate], pack: Option<&[u8]>) -> GitResult<Vec<u8>> {
    if updates.is_empty() {
        return fail("nothing to push");
    }
    let mut out = Vec::new();
    for (i, u) in updates.iter().enumerate() {
        if !valid_ref(&u.name) {
            return fail(format!("refusing ref name {:?}", u.name));
        }
        unhex(&u.old)?;
        unhex(&u.new)?;
        let caps = if i == 0 {
            "\0report-status agent=collective"
        } else {
            ""
        };
        out.extend(pkt_line(
            format!("{} {} {}{caps}\n", u.old, u.new, u.name).as_bytes(),
        ));
    }
    out.extend_from_slice(FLUSH);
    let deletes_only = updates.iter().all(|u| u.new == ZERO_ID);
    match (deletes_only, pack) {
        (true, _) => {}
        (false, Some(p)) => out.extend_from_slice(p),
        (false, None) => return fail("a push that creates or moves refs needs a pack"),
    }
    Ok(out)
}

/// Reads a receive-pack `report-status`: each ref with its error, if any.
/// A pack the server failed to unpack is an error for the whole push.
pub fn parse_report_status(bytes: &[u8]) -> GitResult<Vec<(String, Option<String>)>> {
    let mut lines = pkt_lines(bytes)?.into_iter().filter_map(|p| match p {
        Pkt::Data(d) => Some(d),
        _ => None,
    });
    let Some(first) = lines.next() else {
        return fail("empty push response");
    };
    if let Some(e) = err_line(first) {
        return fail(format!("server error: {e}"));
    }
    match text_line(first).strip_prefix("unpack ") {
        Some("ok") => {}
        Some(e) => return fail(format!("server could not unpack: {e}")),
        None => return fail("malformed push response"),
    }
    let mut out = Vec::new();
    for d in lines {
        let line = text_line(d);
        if let Some(r) = line.strip_prefix("ok ") {
            out.push((r.to_string(), None));
        } else if let Some(rest) = line.strip_prefix("ng ") {
            let (r, why) = rest.split_once(' ').unwrap_or((rest, "rejected"));
            out.push((r.to_string(), Some(why.to_string())));
        } else {
            return fail(format!("malformed push response: {line}"));
        }
    }
    Ok(out)
}

// -- protocol-v2 fetch --------------------------------------------------------

/// The body of a protocol-v2 `git-upload-pack` POST asking for `wants` given
/// that the client already has `haves`. Send with `Git-Protocol: version=2`.
pub fn fetch_request(wants: &[String], haves: &[String]) -> GitResult<Vec<u8>> {
    if wants.is_empty() {
        return fail("nothing to fetch");
    }
    let mut out = pkt_line(b"command=fetch\n");
    out.extend(pkt_line(b"agent=collective\n"));
    out.extend_from_slice(DELIM);
    out.extend(pkt_line(b"no-progress\n"));
    for w in wants {
        unhex(w)?;
        out.extend(pkt_line(format!("want {w}\n").as_bytes()));
    }
    for h in haves {
        unhex(h)?;
        out.extend(pkt_line(format!("have {h}\n").as_bytes()));
    }
    out.extend(pkt_line(b"done\n"));
    out.extend_from_slice(FLUSH);
    Ok(out)
}

/// Extracts the pack from a protocol-v2 fetch response (the `packfile`
/// section, demultiplexed from its side band).
pub fn fetch_response_pack(bytes: &[u8]) -> GitResult<Vec<u8>> {
    let mut pack = Vec::new();
    let mut in_pack = false;
    for p in pkt_lines(bytes)? {
        match p {
            Pkt::Data(d) if !in_pack => {
                if let Some(e) = err_line(d) {
                    return fail(format!("server error: {e}"));
                }
                in_pack = d == b"packfile\n";
            }
            Pkt::Data(d) => match d.first() {
                Some(1) => pack.extend_from_slice(&d[1..]),
                Some(2) => {}
                Some(3) => return fail(format!("server error: {}", text_line(&d[1..]))),
                _ => return fail("malformed side band"),
            },
            Pkt::Flush if in_pack => break,
            _ => {}
        }
    }
    if !pack.starts_with(b"PACK") {
        return fail("the response carried no pack");
    }
    Ok(pack)
}

// -- commits Collective writes ------------------------------------------------

/// The committer on every commit Collective writes.
pub fn collective_signature(time: i64) -> Signature {
    Signature {
        name: "Collective".into(),
        email: "git@collective.software".into(),
        time,
    }
}

/// The commit for a patch: its diff applied to its base, authored by the
/// submitting agent's operator at the patch's creation time.
pub fn patch_commit(
    base: &str,
    tree: &str,
    patch_id: &str,
    title: &str,
    author: &str,
    created_at: i64,
) -> GitResult<Object> {
    let who = Signature {
        name: author.to_string(),
        email: format!("{author}@agents.collective.software"),
        time: created_at,
    };
    let title = title.lines().next().unwrap_or_default().trim();
    let message = format!("{title}\n\nCollective-Patch: {patch_id}\n");
    Ok(Object::new(
        Kind::Commit,
        encode_commit(
            tree,
            &[base.to_string()],
            &who,
            &collective_signature(created_at),
            &message,
        )?,
    ))
}

/// The commit for a trunk: a merge of the base and the commits of the
/// patches it carries (in application order), whose tree is their combined
/// diff applied to the base.
pub fn trunk_commit(
    base: &str,
    tree: &str,
    patch_ids: &[String],
    patch_commits: &[String],
    created_at: i64,
) -> GitResult<Object> {
    let mut parents = vec![base.to_string()];
    parents.extend(patch_commits.iter().cloned());
    let mut message = format!("Trunk for {base}: {} patches\n\n", patch_ids.len());
    for id in patch_ids {
        message.push_str(&format!("Collective-Patch: {id}\n"));
    }
    let sig = collective_signature(created_at);
    Ok(Object::new(
        Kind::Commit,
        encode_commit(tree, &parents, &sig, &sig, &message)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_diff;

    fn one(diff: &str) -> FileDiff {
        parse_diff(diff).unwrap().remove(0)
    }

    fn apply_str(old: Option<&str>, diff: &str) -> GitResult<Option<String>> {
        apply(old.map(str::as_bytes), &one(diff)).map(|o| o.map(|b| String::from_utf8(b).unwrap()))
    }

    #[test]
    fn known_object_ids() {
        assert_eq!(
            object_id(Kind::Blob, b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        assert_eq!(
            Object::new(Kind::Tree, encode_tree(&[]).unwrap()).id,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
    }

    #[test]
    fn modifies_inserts_and_appends() {
        let old = "a\nb\nc\nd\n";
        let d = "--- a/f\n+++ b/f\n@@ -0,0 +1 @@\n+top\n@@ -2,2 +3,2 @@\n b\n-c\n+C\n@@ -4,0 +6 @@\n+end\n";
        assert_eq!(
            apply_str(Some(old), d).unwrap().unwrap(),
            "top\na\nb\nC\nd\nend\n"
        );
    }

    #[test]
    fn rejects_context_that_does_not_match() {
        let d = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n-x\n+y\n";
        assert!(apply_str(Some("a\nb\n"), d).is_err());
        assert!(apply_str(None, d).is_err());
    }

    #[test]
    fn handles_missing_final_newlines() {
        // Adding a newline at the end.
        let d = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+a\n";
        assert_eq!(apply_str(Some("a"), d).unwrap().unwrap(), "a\n");
        // Removing it.
        let d = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+a\n\\ No newline at end of file\n";
        assert_eq!(apply_str(Some("a\n"), d).unwrap().unwrap(), "a");
        // An untouched end keeps what it had.
        let d = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(apply_str(Some("a\nz"), d).unwrap().unwrap(), "b\nz");
    }

    #[test]
    fn creates_and_deletes_files() {
        let d = "--- /dev/null\n+++ b/n\n@@ -0,0 +1,2 @@\n+x\n+y\n";
        assert_eq!(apply_str(None, d).unwrap().unwrap(), "x\ny\n");
        assert!(apply_str(Some("x\n"), d).is_err());
        let d = "--- a/n\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-x\n-y\n";
        assert_eq!(apply_str(Some("x\ny\n"), d).unwrap(), None);
        assert!(apply_str(Some("x\ny\nz\n"), d).is_err());
    }

    #[test]
    fn keeps_crlf_line_endings() {
        let d = "--- a/f\n+++ b/f\n@@ -1,2 +1,3 @@\n a\n+new\n b\n";
        assert_eq!(
            apply_str(Some("a\r\nb\r\n"), d).unwrap().unwrap(),
            "a\r\nnew\r\nb\r\n"
        );
    }

    fn entry(mode: &str, name: &str, id: &str) -> TreeEntry {
        TreeEntry {
            mode: mode.into(),
            name: name.into(),
            id: id.into(),
        }
    }

    #[test]
    fn trees_sort_directories_as_if_slashed() {
        let blob = object_id(Kind::Blob, b"");
        let a = encode_tree(&[
            entry(TREE_MODE, "a", &blob),
            entry(FILE_MODE, "a.txt", &blob),
        ])
        .unwrap();
        let b = encode_tree(&[
            entry(FILE_MODE, "a.txt", &blob),
            entry(TREE_MODE, "a", &blob),
        ])
        .unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with(b"100644 a.txt\0"));
        assert!(encode_tree(&[entry(FILE_MODE, ".git", &blob)]).is_err());
    }

    fn write(content: &str) -> Change {
        Change::Write {
            mode: FILE_MODE.into(),
            content: content.as_bytes().to_vec(),
        }
    }

    #[test]
    fn edits_nested_trees() {
        let keep = object_id(Kind::Blob, b"keep\n");
        let mut trees = BTreeMap::new();
        trees.insert(String::new(), vec![entry(FILE_MODE, "README", &keep)]);
        let mut changes = BTreeMap::new();
        changes.insert("src/deep/new.rs".to_string(), write("fn main() {}\n"));
        assert_eq!(
            dirs_to_read(changes.keys().map(String::as_str)),
            vec!["", "src", "src/deep"]
        );
        let (root, objects) = edit_tree(&trees, &changes).unwrap();
        // blob, src/deep, src, root
        assert_eq!(objects.len(), 4);
        assert_eq!(objects.last().unwrap().id, root);

        // Deleting the only file removes its directories again.
        let src = objects
            .iter()
            .find(|o| o.body.starts_with(b"40000 deep"))
            .unwrap();
        let deep = &objects[1];
        let mut trees = BTreeMap::new();
        trees.insert(
            String::new(),
            vec![
                entry(FILE_MODE, "README", &keep),
                entry(TREE_MODE, "src", &src.id),
            ],
        );
        trees.insert("src".to_string(), vec![entry(TREE_MODE, "deep", &deep.id)]);
        trees.insert(
            "src/deep".to_string(),
            vec![entry(FILE_MODE, "new.rs", &objects[0].id)],
        );
        let mut changes = BTreeMap::new();
        changes.insert("src/deep/new.rs".to_string(), Change::Delete);
        let (root, _) = edit_tree(&trees, &changes).unwrap();
        let only_readme = encode_tree(&[entry(FILE_MODE, "README", &keep)]).unwrap();
        assert_eq!(root, object_id(Kind::Tree, &only_readme));
    }

    #[test]
    fn refuses_unsafe_or_unread_paths() {
        let id = object_id(Kind::Blob, b"");
        let mut trees = BTreeMap::new();
        trees.insert(String::new(), vec![entry(TREE_MODE, "src", &id)]);
        for bad in [".git/config", "a/../b", "/abs", "src/.GIT/hooks/x"] {
            let mut changes = BTreeMap::new();
            changes.insert(bad.to_string(), write("x"));
            assert!(edit_tree(&trees, &changes).is_err(), "{bad}");
        }
        // `src` exists but wasn't read: never rebuild it from nothing.
        let mut changes = BTreeMap::new();
        changes.insert("src/x".to_string(), write("x"));
        assert!(edit_tree(&trees, &changes).is_err());
    }

    #[test]
    fn packs_have_header_count_and_trailer() {
        let o = Object::new(Kind::Blob, vec![b'x'; 300]);
        let pack = write_pack(&[o.clone(), o]);
        assert_eq!(&pack[..4], b"PACK");
        assert_eq!(&pack[8..12], &1u32.to_be_bytes());
        // 300 = 0b1_0010_1100: low nibble 0xc with the continuation bit, then 0x12.
        assert_eq!(&pack[12..14], &[0x80 | 0x30 | 0x0c, 0x12]);
        let (body, trailer) = pack.split_at(pack.len() - 20);
        assert_eq!(trailer, Sha1::digest(body).as_slice());
    }

    #[test]
    fn reads_advertisements_and_reports() {
        let id = "1".repeat(40);
        let mut adv = pkt_line(b"# service=git-receive-pack\n");
        adv.extend_from_slice(FLUSH);
        adv.extend(pkt_line(
            format!("{id} refs/heads/main\0report-status\n").as_bytes(),
        ));
        adv.extend(pkt_line(format!("{id} refs/heads/base/{id}\n").as_bytes()));
        adv.extend_from_slice(FLUSH);
        let refs = parse_advertisement(&adv).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs["refs/heads/main"], id);

        let mut empty =
            pkt_line(format!("{ZERO_ID} capabilities^{{}}\0report-status\n").as_bytes());
        empty.extend_from_slice(FLUSH);
        assert!(parse_advertisement(&empty).unwrap().is_empty());

        let mut rep = pkt_line(b"unpack ok\n");
        rep.extend(pkt_line(b"ok refs/heads/a\n"));
        rep.extend(pkt_line(b"ng refs/heads/b stale info\n"));
        rep.extend_from_slice(FLUSH);
        assert_eq!(
            parse_report_status(&rep).unwrap(),
            vec![
                ("refs/heads/a".to_string(), None),
                ("refs/heads/b".to_string(), Some("stale info".to_string()))
            ]
        );
        let mut bad = pkt_line(b"unpack index-pack failed\n");
        bad.extend_from_slice(FLUSH);
        assert!(parse_report_status(&bad).is_err());
    }

    #[test]
    fn builds_push_requests() {
        let id = "2".repeat(40);
        let up = RefUpdate {
            name: patch_ref(&"a".repeat(64)),
            old: ZERO_ID.into(),
            new: id.clone(),
        };
        assert!(receive_pack_request(std::slice::from_ref(&up), None).is_err());
        let req = receive_pack_request(&[up], Some(b"PACK")).unwrap();
        assert!(req.ends_with(b"0000PACK"));
        let del = RefUpdate {
            name: "refs/heads/patch/x".into(),
            old: id,
            new: ZERO_ID.into(),
        };
        assert!(receive_pack_request(std::slice::from_ref(&del), None).is_ok());
        let sneaky = RefUpdate {
            name: "refs/heads/../config".into(),
            ..del
        };
        assert!(receive_pack_request(&[sneaky], None).is_err());
    }

    #[test]
    fn demultiplexes_fetch_responses() {
        let mut resp = pkt_line(b"packfile\n");
        resp.extend(pkt_line(b"\x01PACK\x00\x00"));
        resp.extend(pkt_line(b"\x02counting objects\n"));
        resp.extend(pkt_line(b"\x01\x00\x02rest"));
        resp.extend_from_slice(FLUSH);
        assert_eq!(
            fetch_response_pack(&resp).unwrap(),
            b"PACK\x00\x00\x00\x02rest"
        );
        let mut err = pkt_line(b"packfile\n");
        err.extend(pkt_line(b"\x03not our ref\n"));
        assert!(fetch_response_pack(&err).is_err());
        assert!(fetch_response_pack(&pkt_line(b"ERR upload-pack: not our ref\n")).is_err());
    }

    #[test]
    fn repo_names_are_safe() {
        assert_eq!(repo_name("ab12/x y"), "p-ab12-x-y");
    }

    #[test]
    fn patch_commits_are_reproducible() {
        let base = "3".repeat(40);
        let tree = Object::new(Kind::Tree, Vec::new()).id;
        let a = patch_commit(&base, &tree, "pid", "Fix it\nmore", "alice", 1_700_000_000).unwrap();
        let b = patch_commit(&base, &tree, "pid", "Fix it\nmore", "alice", 1_700_000_000).unwrap();
        assert_eq!(a.id, b.id);
        let text = String::from_utf8(a.body).unwrap();
        assert!(text.contains(&format!("parent {base}\n")));
        assert!(text.ends_with("\n\nFix it\n\nCollective-Patch: pid\n"));
    }
}
