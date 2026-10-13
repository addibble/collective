//! Git hosting on Cloudflare Artifacts (docs/GIT.md). Each project gets a
//! repository imported from its upstream; every open patch becomes a
//! `patch/<id>` branch (one commit on its base) and every trunk a
//! `trunk/<base>` branch (a merge of the base and its patches). Bases the
//! import didn't carry are fetched from upstream and kept as `base/<sha>`.
//!
//! The project's coordinator runs every write, one at a time;
//! collective_core::git builds the objects and the wire bytes. Hosting
//! mirrors the protocol and nothing depends on it: a failure is recorded and
//! the branch is simply missing. Only open patches are published, so a
//! canary, a withdrawn patch or one under challenge never has a branch.

use std::collections::BTreeMap;

use collective_core::diff::FileDiff;
use collective_core::git::{self as g, Change, Object, RefUpdate, TreeEntry, ZERO_ID};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use worker::js_sys::{self, Array, Function, Promise, Reflect, Uint8Array};
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::wasm_bindgen_futures::JsFuture;
use worker::*;

use crate::args;
use crate::error::{status, ApiError, ApiResult};
use crate::store::{now, parse, Store};

const BINDING: &str = "ARTIFACTS";
/// Largest pack fetched from upstream for a base the import didn't carry.
const MAX_FETCH_BYTES: usize = 64 << 20;
/// Read tokens handed to agents live an hour and are reused until ten
/// minutes before they expire.
const READ_TOKEN_TTL_SECS: i64 = 3600;
const READ_TOKEN_MIN_LEFT_MS: i64 = 600_000;
/// Write tokens live only as long as one job needs them.
const WRITE_TOKEN_TTL_SECS: i64 = 600;
/// Repository refs offered to upstream as `have`s when fetching a base.
const MAX_HAVES: usize = 64;
/// An import that hasn't finished after this long is treated as failed.
const IMPORT_TIMEOUT_MS: i64 = 6 * 3600 * 1000;

/// Work for a project's coordinator. Jobs are idempotent: each one makes the
/// repository match the protocol's current state for its subject.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "lowercase")]
pub enum Job {
    /// Create the project's repository (import it from upstream).
    Repo,
    /// Publish, update or remove a patch's branch.
    Patch(String),
    /// Publish, update or remove the trunk branch for a base.
    Trunk(String),
}

/// Whether this server has an Artifacts binding at all.
pub fn enabled(env: &Env) -> bool {
    Reflect::get(env, &JsValue::from_str(BINDING)).is_ok_and(|b| !b.is_undefined())
}

/// Asks the project's coordinator to run `job`. Hosting never blocks the
/// protocol, so failures are logged, not returned.
pub async fn notify(env: &Env, project_id: &str, job: Job) {
    if !enabled(env) {
        return;
    }
    if let Err(e) = crate::project::enqueue_git(env, project_id, &job).await {
        console_error!("git hosting: could not queue {job:?} for {project_id}: {e}");
    }
}

// -- errors ---------------------------------------------------------------------

pub enum JobError {
    /// Try again later (the repository is still importing).
    Retry(String),
    /// Record and drop: a later event queues the job again.
    Fail(String),
}

impl From<ApiError> for JobError {
    fn from(e: ApiError) -> Self {
        JobError::Fail(e.message)
    }
}

impl From<worker::Error> for JobError {
    fn from(e: worker::Error) -> Self {
        JobError::Fail(e.to_string())
    }
}

impl From<g::GitError> for JobError {
    fn from(e: g::GitError) -> Self {
        JobError::Fail(e.0)
    }
}

impl From<ArtErr> for JobError {
    fn from(e: ArtErr) -> Self {
        JobError::Fail(format!("artifacts {}: {}", e.code, e.message))
    }
}

type JobResult<T> = Result<T, JobError>;

fn fail<T>(msg: impl Into<String>) -> JobResult<T> {
    Err(JobError::Fail(msg.into()))
}

// -- the Artifacts binding (no workers-rs wrapper yet) -------------------------

#[derive(Debug)]
pub struct ArtErr {
    pub code: String,
    pub message: String,
}

fn art_err(e: JsValue) -> ArtErr {
    let field = |k: &str| {
        Reflect::get(&e, &JsValue::from_str(k))
            .ok()
            .and_then(|v| v.as_string())
    };
    ArtErr {
        code: field("code").unwrap_or_else(|| "INTERNAL_ERROR".into()),
        message: field("message").unwrap_or_else(|| format!("{e:?}")),
    }
}

/// Calls `target.method(...args)` and awaits the result (RPC calls return
/// thenables, so it's always passed through `Promise.resolve`).
async fn call(target: &JsValue, method: &str, args: &[JsValue]) -> Result<JsValue, ArtErr> {
    let f = Reflect::get(target, &JsValue::from_str(method)).map_err(art_err)?;
    if !f.is_function() {
        return Err(ArtErr {
            code: "INTERNAL_ERROR".into(),
            message: format!("{method} is not a function"),
        });
    }
    let list = Array::new();
    for a in args {
        list.push(a);
    }
    // Reflect.apply, not f.apply(): on an RPC stub every property, `apply`
    // included, is a remote method.
    let ret = Reflect::apply(f.unchecked_ref::<Function>(), target, &list).map_err(art_err)?;
    JsFuture::from(Promise::resolve(&ret))
        .await
        .map_err(art_err)
}

fn from_js<T: DeserializeOwned>(v: &JsValue) -> Result<Option<T>, ArtErr> {
    if v.is_null() || v.is_undefined() {
        return Ok(None);
    }
    let text = js_sys::JSON::stringify(v)
        .ok()
        .and_then(|s| s.as_string())
        .unwrap_or_default();
    serde_json::from_str(&text).map(Some).map_err(|e| ArtErr {
        code: "INTERNAL_ERROR".into(),
        message: format!("unexpected value from Artifacts: {e}"),
    })
}

fn obj(pairs: &[(&str, JsValue)]) -> JsValue {
    let o = js_sys::Object::new();
    for (k, v) in pairs {
        let _ = Reflect::set(&o, &JsValue::from_str(k), v);
    }
    o.into()
}

fn namespace(env: &Env) -> Result<JsValue, ArtErr> {
    Reflect::get(env, &JsValue::from_str(BINDING))
        .ok()
        .filter(|b| !b.is_undefined())
        .ok_or_else(|| ArtErr {
            code: "NOT_CONFIGURED".into(),
            message: "no ARTIFACTS binding".into(),
        })
}

/// A repository capability (an RPC stub). It is released when the request
/// or alarm that opened it ends.
struct Repo(JsValue);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitMeta {
    tree_hash: String,
}

#[derive(Deserialize)]
struct RepoInfo {
    remote: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Token {
    plaintext: String,
}

impl Repo {
    async fn open(env: &Env, name: &str) -> Result<Repo, ArtErr> {
        Ok(Repo(
            call(&namespace(env)?, "get", &[JsValue::from_str(name)]).await?,
        ))
    }

    async fn info(&self) -> Result<RepoInfo, ArtErr> {
        from_js(&call(&self.0, "info", &[]).await?)?.ok_or_else(|| ArtErr {
            code: "NOT_FOUND".into(),
            message: "repository info missing".into(),
        })
    }

    async fn token(&self, scope: &str, ttl_secs: i64) -> Result<String, ArtErr> {
        let t: Option<Token> = from_js(
            &call(
                &self.0,
                "createToken",
                &[JsValue::from_str(scope), JsValue::from_f64(ttl_secs as f64)],
            )
            .await?,
        )?;
        t.map(|t| t.plaintext).ok_or_else(|| ArtErr {
            code: "INTERNAL_ERROR".into(),
            message: "no token returned".into(),
        })
    }

    async fn commit_tree(&self, commit: &str) -> Result<Option<String>, ArtErr> {
        let c: Option<CommitMeta> =
            from_js(&call(&self.0, "readCommit", &[JsValue::from_str(commit)]).await?)?;
        Ok(c.map(|c| c.tree_hash))
    }

    async fn tree(&self, id: &str) -> Result<Option<Vec<TreeEntry>>, ArtErr> {
        #[derive(Deserialize)]
        struct E {
            mode: String,
            name: String,
            hash: String,
        }
        let entries: Option<Vec<E>> =
            from_js(&call(&self.0, "readTree", &[JsValue::from_str(id)]).await?)?;
        Ok(entries.map(|es| {
            es.into_iter()
                .map(|e| TreeEntry {
                    // Trees store directory modes without the leading zero.
                    mode: e.mode.trim_start_matches('0').to_string(),
                    name: e.name,
                    id: e.hash,
                })
                .collect()
        }))
    }

    async fn blob(&self, id: &str) -> Result<Option<Vec<u8>>, ArtErr> {
        let b = call(&self.0, "readBlob", &[JsValue::from_str(id)]).await?;
        if b.is_null() || b.is_undefined() {
            return Ok(None);
        }
        let buf = call(&b, "arrayBuffer", &[]).await?;
        Ok(Some(Uint8Array::new(&buf).to_vec()))
    }
}

// -- smart HTTP ------------------------------------------------------------------

async fn http(
    url: &str,
    token: Option<&str>,
    body: Option<(&str, Vec<u8>)>,
    v2: bool,
) -> JobResult<Vec<u8>> {
    let headers = Headers::new();
    headers.set("user-agent", "git/2.0 (collective)")?;
    if let Some(t) = token {
        headers.set("authorization", &format!("Bearer {t}"))?;
    }
    if v2 {
        headers.set("git-protocol", "version=2")?;
    }
    let mut init = RequestInit::new();
    match body {
        Some((service, bytes)) => {
            headers.set("content-type", &format!("application/x-{service}-request"))?;
            headers.set("accept", &format!("application/x-{service}-result"))?;
            init.with_method(Method::Post)
                .with_body(Some(JsValue::from(Uint8Array::from(bytes.as_slice()))));
        }
        None => {
            init.with_method(Method::Get);
        }
    }
    init.with_headers(headers);
    let mut resp = Fetch::Request(Request::new_with_init(url, &init)?)
        .send()
        .await?;
    let declared = resp
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > MAX_FETCH_BYTES) {
        return fail(format!(
            "{url}: response larger than {} MB",
            MAX_FETCH_BYTES >> 20
        ));
    }
    let code = resp.status_code();
    let bytes = resp.bytes().await?;
    if code != 200 {
        let text = String::from_utf8_lossy(&bytes[..bytes.len().min(300)]).to_string();
        return fail(format!("{url}: HTTP {code} {text}"));
    }
    if bytes.len() > MAX_FETCH_BYTES {
        return fail(format!(
            "{url}: response larger than {} MB",
            MAX_FETCH_BYTES >> 20
        ));
    }
    Ok(bytes)
}

/// A repository as the coordinator works on it: the capability, its remote
/// and a short-lived write token.
struct Host {
    project_id: String,
    upstream: String,
    repo: Repo,
    remote: String,
    token: String,
}

impl Host {
    async fn refs(&self) -> JobResult<BTreeMap<String, String>> {
        let adv = http(
            &format!("{}/info/refs?service=git-receive-pack", self.remote),
            Some(&self.token),
            None,
            false,
        )
        .await?;
        Ok(g::parse_advertisement(&adv)?)
    }

    async fn push(&self, updates: &[RefUpdate], objects: &[Object]) -> JobResult<()> {
        let pack = updates
            .iter()
            .any(|u| u.new != ZERO_ID)
            .then(|| g::write_pack(objects));
        let req = g::receive_pack_request(updates, pack.as_deref())?;
        let resp = http(
            &format!("{}/git-receive-pack", self.remote),
            Some(&self.token),
            Some(("git-receive-pack", req)),
            false,
        )
        .await?;
        let report = g::parse_report_status(&resp)?;
        let errors: Vec<String> = report
            .into_iter()
            .filter_map(|(r, e)| e.map(|e| format!("{r}: {e}")))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            fail(format!("push rejected: {}", errors.join("; ")))
        }
    }

    /// Moves `name` from `old` to `new`, recreating it when the host refuses
    /// a non-fast-forward update (trunks are rebuilt, not extended).
    async fn set_ref(
        &self,
        name: &str,
        old: Option<&String>,
        new: &str,
        objects: &[Object],
    ) -> JobResult<()> {
        let update = RefUpdate {
            name: name.to_string(),
            old: old.cloned().unwrap_or_else(|| ZERO_ID.into()),
            new: new.to_string(),
        };
        match self.push(std::slice::from_ref(&update), objects).await {
            Err(JobError::Fail(e)) if old.is_some() && e.contains("fast-forward") => {
                self.delete_ref(name, old).await?;
                let create = RefUpdate {
                    old: ZERO_ID.into(),
                    ..update
                };
                self.push(&[create], objects).await
            }
            r => r,
        }
    }

    async fn delete_ref(&self, name: &str, old: Option<&String>) -> JobResult<()> {
        let Some(old) = old else { return Ok(()) };
        self.push(
            &[RefUpdate {
                name: name.to_string(),
                old: old.clone(),
                new: ZERO_ID.into(),
            }],
            &[],
        )
        .await
    }

    /// The base commit's root tree, fetching the base from upstream into
    /// `base/<sha>` when the repository doesn't have it yet.
    async fn base_tree(&self, base: &str, refs: &BTreeMap<String, String>) -> JobResult<String> {
        if let Some(t) = self.repo.commit_tree(base).await? {
            return Ok(t);
        }
        // Offer what the repository already has from upstream, so only the
        // missing history comes over. Our own branches are never upstream's.
        let mut haves: Vec<String> = refs
            .iter()
            .filter(|(n, _)| {
                !n.starts_with("refs/heads/patch/") && !n.starts_with("refs/heads/trunk/")
            })
            .map(|(_, id)| id.clone())
            .collect();
        haves.sort();
        haves.dedup();
        haves.truncate(MAX_HAVES);
        let req = g::fetch_request(&[base.to_string()], &haves)?;
        let resp = http(
            &format!("https://{}.git/git-upload-pack", self.upstream),
            None,
            Some(("git-upload-pack", req)),
            true,
        )
        .await?;
        let pack = g::fetch_response_pack(&resp)?;
        let update = RefUpdate {
            name: g::base_ref(base),
            old: ZERO_ID.into(),
            new: base.to_string(),
        };
        let req = g::receive_pack_request(&[update], Some(&pack))?;
        let resp = http(
            &format!("{}/git-receive-pack", self.remote),
            Some(&self.token),
            Some(("git-receive-pack", req)),
            false,
        )
        .await?;
        if let Some((r, Some(e))) = g::parse_report_status(&resp)?.into_iter().next() {
            return fail(format!("could not store base {base} ({r}: {e})"));
        }
        match self.repo.commit_tree(base).await? {
            Some(t) => Ok(t),
            None => fail(format!("base {base} is missing after fetching it")),
        }
    }

    /// Applies `files` to the tree `root` and returns the new tree's id with
    /// the objects to push.
    async fn apply(&self, root: &str, files: &[FileDiff]) -> JobResult<(String, Vec<Object>)> {
        let dirs = g::dirs_to_read(files.iter().map(FileDiff::path));
        let mut trees: BTreeMap<String, Vec<TreeEntry>> = BTreeMap::new();
        for dir in dirs {
            let id = if dir.is_empty() {
                Some(root.to_string())
            } else {
                let (parent, name) = dir.rsplit_once('/').unwrap_or(("", &dir));
                trees
                    .get(parent)
                    .and_then(|es| es.iter().find(|e| e.name == name && e.mode == g::TREE_MODE))
                    .map(|e| e.id.clone())
            };
            if let Some(id) = id {
                match self.repo.tree(&id).await? {
                    Some(entries) => {
                        trees.insert(dir, entries);
                    }
                    None => return fail(format!("tree {id} is missing")),
                }
            }
        }
        let mut changes = BTreeMap::new();
        for f in files {
            let path = f.path();
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
            let entry = trees
                .get(parent)
                .and_then(|es| es.iter().find(|e| e.name == name && e.mode != g::TREE_MODE));
            if entry.is_some_and(|e| e.mode == "160000") {
                return fail(format!("{path} is a submodule"));
            }
            let old = match entry {
                Some(e) => Some(
                    self.repo
                        .blob(&e.id)
                        .await?
                        .ok_or_else(|| JobError::Fail(format!("blob for {path} is missing")))?,
                ),
                None => None,
            };
            let change = match g::apply(old.as_deref(), f)? {
                Some(content) => Change::Write {
                    mode: entry
                        .map(|e| e.mode.clone())
                        .or_else(|| f.new_mode.clone())
                        .unwrap_or_else(|| g::FILE_MODE.into()),
                    content,
                },
                None => Change::Delete,
            };
            changes.insert(path.to_string(), change);
        }
        Ok(g::edit_tree(&trees, &changes)?)
    }
}

// -- repositories ---------------------------------------------------------------

#[derive(Deserialize)]
struct RepoRow {
    name: String,
    remote: Option<String>,
    status: String,
    error: Option<String>,
    read_token: Option<String>,
    read_token_expires: Option<f64>,
    created_at: f64,
}

async fn repo_row(store: &Store, project_id: &str) -> ApiResult<Option<RepoRow>> {
    store
        .db
        .first::<RepoRow>(
            "SELECT name, remote, status, error, read_token, read_token_expires, created_at FROM git_repos WHERE project_id = ?",
            args![project_id],
        )
        .await
}

async fn set_repo_status(
    store: &Store,
    project_id: &str,
    status: &str,
    error: Option<&str>,
    remote: Option<&str>,
) -> ApiResult<()> {
    store
        .db
        .run(
            "UPDATE git_repos SET status = ?, error = ?, remote = COALESCE(?, remote), updated_at = ? WHERE project_id = ?",
            args![status, error, remote, now(), project_id],
        )
        .await
}

/// Opens the project's repository, starting its import on first use.
async fn open(store: &Store, env: &Env, project_id: &str) -> JobResult<Host> {
    #[derive(Deserialize)]
    struct P {
        upstream_url: String,
    }
    let Some(p) = store
        .db
        .first::<P>(
            "SELECT upstream_url FROM projects WHERE id = ?",
            args![project_id],
        )
        .await?
    else {
        return fail("unknown project");
    };
    // Like proofs, hosting only reaches out to GitHub: the upstream URL comes
    // from whoever submitted the first patch.
    if !p.upstream_url.starts_with("github.com/") {
        return fail("git hosting covers GitHub projects only");
    }
    let row = match repo_row(store, project_id).await? {
        Some(r) => r,
        None => {
            let name = g::repo_name(project_id);
            let params = obj(&[
                (
                    "source",
                    obj(&[(
                        "url",
                        JsValue::from_str(&format!("https://{}", p.upstream_url)),
                    )]),
                ),
                (
                    "target",
                    obj(&[
                        ("name", JsValue::from_str(&name)),
                        (
                            "opts",
                            obj(&[(
                                "description",
                                JsValue::from_str(&format!("Collective: {}", p.upstream_url)),
                            )]),
                        ),
                    ]),
                ),
            ]);
            let (status, error) = match call(&namespace(env)?, "import", &[params]).await {
                Ok(_) => ("importing", None),
                Err(e) if e.code == "ALREADY_EXISTS" => ("importing", None),
                Err(e) => (
                    "failed",
                    Some(format!("import failed: {} {}", e.code, e.message)),
                ),
            };
            let t = now();
            store
                .db
                .run(
                    "INSERT OR IGNORE INTO git_repos (project_id, name, status, error, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
                    args![project_id, &name, status, error.as_deref(), t, t],
                )
                .await?;
            return match error {
                Some(e) => fail(e),
                None => Err(JobError::Retry("repository import started".into())),
            };
        }
    };
    if row.status == "failed" {
        return fail(row.error.unwrap_or_else(|| "repository unavailable".into()));
    }
    let repo = match Repo::open(env, &row.name).await {
        Ok(r) => r,
        Err(e) if row.status == "importing" => {
            if now() - row.created_at as i64 > IMPORT_TIMEOUT_MS {
                let msg = format!("import did not finish: {} {}", e.code, e.message);
                set_repo_status(store, project_id, "failed", Some(&msg), None).await?;
                return fail(msg);
            }
            return Err(JobError::Retry(format!("still importing ({})", e.code)));
        }
        Err(e) => return Err(e.into()),
    };
    let remote = match (&row.remote, row.status.as_str()) {
        (Some(r), "ready") => r.clone(),
        _ => {
            let info = repo.info().await?;
            set_repo_status(store, project_id, "ready", None, Some(&info.remote)).await?;
            info.remote
        }
    };
    let token = repo.token("write", WRITE_TOKEN_TTL_SECS).await?;
    Ok(Host {
        project_id: project_id.to_string(),
        upstream: p.upstream_url,
        repo,
        remote: remote.trim_end_matches('/').to_string(),
        token,
    })
}

/// Records a branch's state; `branch` is (ref name, kind, subject).
async fn record(
    store: &Store,
    project_id: &str,
    branch: (&str, &str, &str),
    commit: Option<&str>,
    status: &str,
    error: Option<&str>,
) -> ApiResult<()> {
    store
        .db
        .run(
            "INSERT OR REPLACE INTO git_refs (project_id, ref_name, kind, subject, commit_id, status, error, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            args![project_id, branch.0, branch.1, branch.2, commit, status, error, now()],
        )
        .await
}

// -- jobs -------------------------------------------------------------------------

/// Runs one job. `Retry` means keep it queued; anything else is finished
/// (failures are recorded against the ref).
pub async fn run(env: &Env, project_id: &str, job: &Job) -> Result<(), String> {
    let store = Store::new(env).map_err(|e| e.message)?;
    let (name, kind, subject) = match job {
        Job::Repo => (String::new(), "repo", String::new()),
        Job::Patch(id) => (g::patch_ref(id), "patch", id.clone()),
        Job::Trunk(base) => (g::trunk_ref(base), "trunk", base.clone()),
    };
    let result = match open(&store, env, project_id).await {
        Ok(host) => match job {
            Job::Repo => Ok(()),
            Job::Patch(id) => publish_patch(&store, &host, id).await,
            Job::Trunk(base) => publish_trunk(&store, &host, base).await,
        },
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => Ok(()),
        Err(JobError::Retry(why)) => Err(why),
        Err(JobError::Fail(why)) => {
            console_error!("git hosting: {job:?} for {project_id}: {why}");
            if !name.is_empty() {
                let _ = record(
                    &store,
                    project_id,
                    (&name, kind, &subject),
                    None,
                    "failed",
                    Some(&why),
                )
                .await;
            }
            Ok(())
        }
    }
}

async fn publish_patch(store: &Store, host: &Host, id: &str) -> JobResult<()> {
    let Some(p) = store.patch_row(id).await? else {
        return Ok(());
    };
    if p.project_id != host.project_id {
        return fail("patch belongs to another project");
    }
    let name = g::patch_ref(id);
    let refs = host.refs().await?;
    let current = refs.get(&name);
    if !matches!(p.status.as_str(), "open" | "converged") {
        host.delete_ref(&name, current).await?;
        record(
            store,
            &host.project_id,
            (&name, "patch", id),
            None,
            "removed",
            Some(&p.status),
        )
        .await?;
        return Ok(());
    }
    if !g::is_object_id(&p.base_commit) {
        record(
            store,
            &host.project_id,
            (&name, "patch", id),
            None,
            "skipped",
            Some("base_commit is not a full 40-character SHA"),
        )
        .await?;
        return Ok(());
    }
    let root = host.base_tree(&p.base_commit, &refs).await?;
    let files = parse(&store.diff_text(id).await?)?;
    let (tree, mut objects) = host.apply(&root, &files).await?;
    #[derive(Deserialize)]
    struct H {
        handle: String,
    }
    let author = store
        .db
        .first::<H>(
            "SELECT handle FROM operators WHERE id = ?",
            args![&p.author_op],
        )
        .await?
        .map(|h| h.handle)
        .unwrap_or_else(|| "unknown".into());
    let commit = g::patch_commit(
        &p.base_commit,
        &tree,
        id,
        &p.title,
        &author,
        p.created_at as i64 / 1000,
    )?;
    if current != Some(&commit.id) {
        objects.push(commit.clone());
        host.set_ref(&name, current, &commit.id, &objects).await?;
    }
    record(
        store,
        &host.project_id,
        (&name, "patch", id),
        Some(&commit.id),
        "published",
        None,
    )
    .await?;
    Ok(())
}

async fn publish_trunk(store: &Store, host: &Host, base: &str) -> JobResult<()> {
    #[derive(Deserialize)]
    struct T {
        patch_ids: String,
        created_at: f64,
    }
    let Some(t) = store
        .db
        .first::<T>(
            "SELECT patch_ids, created_at FROM trunks WHERE project_id = ? AND base_commit = ?",
            args![&host.project_id, base],
        )
        .await?
    else {
        return Ok(());
    };
    let ids: Vec<String> = serde_json::from_str(&t.patch_ids).unwrap_or_default();
    let name = g::trunk_ref(base);
    let refs = host.refs().await?;
    let current = refs.get(&name);
    if ids.is_empty() {
        host.delete_ref(&name, current).await?;
        record(
            store,
            &host.project_id,
            (&name, "trunk", base),
            None,
            "removed",
            Some("no patches converged"),
        )
        .await?;
        return Ok(());
    }
    if !g::is_object_id(base) {
        record(
            store,
            &host.project_id,
            (&name, "trunk", base),
            None,
            "skipped",
            Some("base_commit is not a full 40-character SHA"),
        )
        .await?;
        return Ok(());
    }
    let root = host.base_tree(base, &refs).await?;
    let files = parse(&store.trunk_diff(&host.project_id, base).await?)?;
    let (tree, mut objects) = host.apply(&root, &files).await?;
    // Merge parents: the patches' own commits, where their branches exist.
    let parents: Vec<String> = ids
        .iter()
        .filter_map(|id| refs.get(&g::patch_ref(id)).cloned())
        .collect();
    let commit = g::trunk_commit(base, &tree, &ids, &parents, t.created_at as i64 / 1000)?;
    if current != Some(&commit.id) {
        objects.push(commit.clone());
        host.set_ref(&name, current, &commit.id, &objects).await?;
    }
    record(
        store,
        &host.project_id,
        (&name, "trunk", base),
        Some(&commit.id),
        "published",
        None,
    )
    .await?;
    Ok(())
}

// -- access for agents -----------------------------------------------------------

#[derive(Deserialize)]
struct RefRow {
    ref_name: String,
    commit_id: Option<String>,
    status: String,
    error: Option<String>,
    updated_at: f64,
}

async fn ref_view(store: &Store, project_id: &str, name: &str) -> ApiResult<Option<Value>> {
    let row = store
        .db
        .first::<RefRow>(
            "SELECT ref_name, commit_id, status, error, updated_at FROM git_refs WHERE project_id = ? AND ref_name = ?",
            args![project_id, name],
        )
        .await?;
    Ok(row.map(|r| {
        json!({
            "ref": r.ref_name,
            "branch": r.ref_name.trim_start_matches("refs/heads/"),
            "commit": r.commit_id,
            "status": r.status,
            "error": r.error,
            "updated_at": r.updated_at as i64,
        })
    }))
}

/// The project's Git remote with a read token, and the state of the branch
/// for `patch` or the trunk for `base` when asked. Queues whatever is
/// missing, so asking is also how older patches get their branches.
pub async fn access(
    store: &Store,
    env: &Env,
    project_ref: &str,
    patch: Option<&str>,
    base: Option<&str>,
) -> ApiResult<Value> {
    if !enabled(env) {
        return status(503, "git hosting is not configured on this server");
    }
    let project = store.require_project(project_ref, false).await?;
    let mut out = json!({
        "project_id": project.id,
        "upstream": project.upstream_url,
        "branches": {
            "patch/<patch id>": "one commit: the patch applied to its base",
            "trunk/<base commit>": "the converged trunk: a merge of the base and its patches",
            "base/<base commit>": "an upstream base the import didn't carry",
        },
    });
    let mut wanted = Vec::new();
    if let Some(id) = patch {
        let p = store.require_patch(id).await?;
        if p.project_id != project.id {
            return status(404, "unknown patch for this project");
        }
        let name = g::patch_ref(id);
        let view = ref_view(store, &project.id, &name).await?;
        if view.is_none() {
            wanted.push(Job::Patch(id.to_string()));
        }
        out["patch"] = view.unwrap_or_else(|| json!({ "ref": name, "status": "queued" }));
    }
    if let Some(b) = base {
        let b = b.to_lowercase();
        let name = g::trunk_ref(&b);
        let view = ref_view(store, &project.id, &name).await?;
        if view.is_none() {
            wanted.push(Job::Trunk(b.clone()));
        }
        out["trunk"] = view.unwrap_or_else(|| json!({ "ref": name, "status": "queued" }));
    }

    let row = repo_row(store, &project.id).await?;
    let ready = row.as_ref().is_some_and(|r| r.status == "ready");
    if row.is_none() {
        wanted.insert(0, Job::Repo);
    }
    for job in wanted {
        notify(env, &project.id, job).await;
    }
    let Some(row) = row.filter(|_| ready) else {
        let (status, error) = match repo_row(store, &project.id).await? {
            Some(r) => (r.status, r.error),
            None => ("importing".to_string(), None),
        };
        out["status"] = json!(status);
        out["error"] = json!(error);
        out["hint"] = json!("The repository is being prepared from upstream. Ask again in a few minutes; until then use the diffs (collective_get_patch, collective_trunk).");
        return Ok(out);
    };
    let remote = row.remote.clone().unwrap_or_default();
    let (token, expires) = match (&row.read_token, row.read_token_expires) {
        (Some(t), Some(exp)) if exp as i64 - now() > READ_TOKEN_MIN_LEFT_MS => {
            (t.clone(), exp as i64)
        }
        _ => {
            let repo = Repo::open(env, &row.name)
                .await
                .map_err(|e| ApiError::new(502, format!("artifacts {}: {}", e.code, e.message)))?;
            let t = repo
                .token("read", READ_TOKEN_TTL_SECS)
                .await
                .map_err(|e| ApiError::new(502, format!("artifacts {}: {}", e.code, e.message)))?;
            let exp = now() + READ_TOKEN_TTL_SECS * 1000;
            store
                .db
                .run(
                    "UPDATE git_repos SET read_token = ?, read_token_expires = ? WHERE project_id = ?",
                    args![&t, exp, &project.id],
                )
                .await?;
            (t, exp)
        }
    };
    let example = match (patch, base) {
        (Some(id), _) => format!("patch/{id}"),
        (None, Some(b)) => format!("trunk/{}", b.to_lowercase()),
        _ => "patch/<patch id>".into(),
    };
    out["status"] = json!("ready");
    out["remote"] = json!(remote);
    out["token"] = json!(token);
    out["token_expires_at"] = json!(expires);
    out["fetch"] = json!(format!(
        "git -c http.extraHeader=\"Authorization: Bearer $TOKEN\" fetch {remote} {example}"
    ));
    out["note"] = json!("The token is read-only and expires; ask again for a fresh one. Pass it as a header, not in the remote URL. A branch is the same code as the diff: read it before you use it.");
    Ok(out)
}

/// Queues a sync of every open patch and trunk of a project (admin; for
/// projects that existed before hosting was turned on).
pub async fn sync_all(store: &Store, env: &Env, project_ref: &str) -> ApiResult<Value> {
    if !enabled(env) {
        return status(503, "git hosting is not configured on this server");
    }
    let project = store.require_project(project_ref, false).await?;
    #[derive(Deserialize)]
    struct I {
        id: String,
    }
    let patches: Vec<I> = store
        .db
        .all(
            "SELECT id FROM patches WHERE project_id = ? AND status IN ('open','converged')",
            args![&project.id],
        )
        .await?;
    #[derive(Deserialize)]
    struct B {
        base_commit: String,
    }
    let trunks: Vec<B> = store
        .db
        .all(
            "SELECT base_commit FROM trunks WHERE project_id = ?",
            args![&project.id],
        )
        .await?;
    let mut jobs = vec![Job::Repo];
    jobs.extend(patches.iter().map(|p| Job::Patch(p.id.clone())));
    jobs.extend(trunks.iter().map(|t| Job::Trunk(t.base_commit.clone())));
    let n = jobs.len();
    for job in jobs {
        crate::project::enqueue_git(env, &project.id, &job)
            .await
            .map_err(|e| ApiError::new(500, e.to_string()))?;
    }
    Ok(json!({ "project_id": project.id, "queued": n }))
}
