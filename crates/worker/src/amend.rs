//! Amendments over D1: a reviewer's fix on top of the patch it is
//! reviewing, the panel's votes on it, and the derived patch an accepted
//! one becomes. The rules (what an amendment may be, who votes, what the
//! votes decide) are in collective_core::amend.
//!
//! An amendment is attached to a task by a reviewer holding it, before
//! the panel is complete. The other reviewers see it with the task and
//! vote once their own review is in. Nobody on the panel learns who
//! amended until acceptance makes the derived patch public, and no tally
//! is shown while it is pending, so voters don't herd and a canary's
//! amendments look like any other's: on a canary nothing is ever accepted
//! or rejected, every amendment lapses when the panel is done.

use collective_core::amend::{
    amended_title, check_amendment, check_evidence, decide, entries, may_vote, Category, Checked,
    Evidence, State, Verdict, Vote, MAX_AMENDMENTS_PER_REVIEWER, MAX_NOTE_CHARS,
};
use collective_core::diff::{format_diff, patch_id, FileDiff, Hunk};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::args;
use crate::error::{bad, status, ApiError, ApiResult};
use crate::review::{load_shard, load_task, submitted_reviews, ShardRow};
use crate::store::{new_id, now, parse, Agent, NewPatch, PatchRow, Store};

#[derive(Debug, Clone, Deserialize)]
pub struct Row {
    pub id: String,
    pub shard_id: String,
    pub patch_id: String,
    pub operator_id: String,
    pub agent_id: String,
    pub category: String,
    pub evidence: String,
    pub amended_id: String,
    pub lines: f64,
    pub state: String,
    pub result_patch_id: Option<String>,
    pub created_at: f64,
    pub resolved_at: Option<f64>,
}

/// `POST /v1/reviews/:task/amendments`, `collective_review_amend`.
#[derive(Debug, Deserialize)]
pub struct Submission {
    pub category: String,
    /// The whole patch with the fix applied: a diff against the task's
    /// base commit.
    pub diff: String,
    pub evidence: Value,
}

/// `POST /v1/reviews/:task/amendments/:id/votes`, `collective_amendment_vote`.
#[derive(Debug, Deserialize)]
pub struct VoteInput {
    pub verdict: Verdict,
    #[serde(default)]
    pub note: Option<String>,
}

/// A vote carried in a review submission (`amendments` in
/// `collective_review_submit`).
#[derive(Debug, Deserialize)]
pub struct ReviewVote {
    pub id: String,
    pub verdict: Verdict,
    #[serde(default)]
    pub note: Option<String>,
}

const SELECT: &str = "SELECT id, shard_id, patch_id, operator_id, agent_id, category, evidence, amended_id, lines, state, result_patch_id, created_at, resolved_at FROM amendments";

async fn load(store: &Store, id: &str) -> ApiResult<Row> {
    match store
        .db
        .first::<Row>(&format!("{SELECT} WHERE id = ?"), args![id])
        .await?
    {
        Some(r) => Ok(r),
        None => status(404, "unknown amendment"),
    }
}

async fn on_shard(store: &Store, shard_id: &str) -> ApiResult<Vec<Row>> {
    store
        .db
        .all(
            &format!("{SELECT} WHERE shard_id = ? ORDER BY created_at, id"),
            args![shard_id],
        )
        .await
}

/// Operators whose review of the task is in: the panel that votes.
async fn panel(store: &Store, shard_id: &str) -> ApiResult<Vec<String>> {
    Ok(submitted_reviews(store, shard_id)
        .await?
        .into_iter()
        .map(|r| r.reviewer)
        .collect())
}

fn blob_key(id: &str) -> String {
    format!("amendments/{id}.diff")
}

async fn amended_files(store: &Store, id: &str) -> ApiResult<Vec<FileDiff>> {
    match store.blobs.get(blob_key(id)).execute().await? {
        Some(obj) => parse(
            &obj.body()
                .ok_or_else(|| ApiError::new(500, "empty amendment blob"))?
                .text()
                .await?,
        ),
        None => status(404, "unknown amendment"),
    }
}

fn hunks_text(hunks: &[Hunk]) -> String {
    let range = |start: u32, len: u32| {
        if len == 1 {
            start.to_string()
        } else {
            format!("{start},{len}")
        }
    };
    let mut out = String::new();
    for h in hunks {
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
    out
}

/// What the amendment changes about the patch, per file, as diff text the
/// panel reads side by side: the patch's hunks it replaces and the hunks
/// that take their place.
fn changes(c: &Checked) -> Value {
    json!(c
        .deltas
        .iter()
        .map(|d| json!({ "path": d.path, "patch": hunks_text(&d.replaced), "amended": hunks_text(&d.amended) }))
        .collect::<Vec<_>>())
}

/// An amendment as a reviewer sees it. No tally and no amender: a voter
/// judges the fix, not its support, and a canary's amendments must look
/// like a real patch's (module docs).
async fn view(store: &Store, row: &Row, viewer: &str, with_diff: bool) -> ApiResult<Value> {
    let evidence: Value = serde_json::from_str(&row.evidence).unwrap_or(Value::Null);
    let mut v = json!({
        "id": row.id,
        "task_id": row.shard_id,
        "patch_id": row.patch_id,
        "category": row.category,
        "evidence": evidence,
        "lines": row.lines as u32,
        "state": row.state,
        "result_patch_id": row.result_patch_id,
        "created_at": row.created_at as i64,
        "resolved_at": row.resolved_at.map(|t| t as i64),
        "mine": row.operator_id == viewer,
    });
    if row.operator_id != viewer {
        #[derive(Deserialize)]
        struct V {
            verdict: String,
        }
        v["my_vote"] = json!(store
            .db
            .first::<V>(
                "SELECT verdict FROM amendment_votes WHERE amendment_id = ? AND operator_id = ?",
                args![&row.id, viewer],
            )
            .await?
            .map(|x| x.verdict));
    }
    if with_diff {
        let patch = store.require_patch_any(&row.patch_id).await?;
        let files = parse(&store.diff_text(&patch.id).await?)?;
        let amended = amended_files(store, &row.id).await?;
        if let Ok(c) = check_amendment(&files, &amended) {
            v["changes"] = changes(&c);
        }
        v["diff"] = json!(format_diff(&amended));
    }
    Ok(v)
}

/// The amendments on a task, for a reviewer that holds or has reviewed it.
/// Included in the task view; `GET /v1/reviews/:task/amendments`.
pub async fn list(store: &Store, agent: &Agent, shard_id: &str) -> ApiResult<Value> {
    load_shard(store, shard_id).await?;
    if load_task(store, shard_id, &agent.operator_id)
        .await?
        .is_none()
    {
        return status(
            403,
            "only reviewers of this task see its amendments; call collective_review_next",
        );
    }
    Ok(json!({ "amendments": for_task(store, shard_id, &agent.operator_id).await? }))
}

/// The amendments on a task as `viewer` sees them, with their diffs.
pub async fn for_task(store: &Store, shard_id: &str, viewer: &str) -> ApiResult<Vec<Value>> {
    let mut out = Vec::new();
    for row in on_shard(store, shard_id).await? {
        out.push(view(store, &row, viewer, true).await?);
    }
    Ok(out)
}

/// Attaches an amendment to a task the reviewer holds, while the panel is
/// still forming. The diff is the whole patch with the fix applied; what
/// it changes about the patch is derived and checked here.
pub async fn submit(
    store: &Store,
    agent: &Agent,
    shard_id: &str,
    sub: Submission,
) -> ApiResult<Value> {
    let shard = load_shard(store, shard_id).await?;
    let me = &agent.operator_id;
    match load_task(store, shard_id, me).await? {
        Some(t) if (t.phase == "review" || t.phase == "run") && t.expires_at as i64 >= now() => {}
        _ => {
            return status(
                403,
                "attach amendments to a task you hold, before its lease expires; call collective_review_next",
            )
        }
    }
    if shard.state != "open" || panel(store, shard_id).await?.len() >= shard.wanted as usize {
        return status(
            409,
            "this task's panel is complete, so nobody is left to vote on an amendment. Submit the fix as a patch of its own with collective_submit_patch, with parent_id set to this patch",
        );
    }
    let Some(category) = Category::parse(&sub.category) else {
        return bad("category must be performance or correctness");
    };
    let mut ev = sub.evidence;
    let Some(obj) = ev.as_object_mut() else {
        return bad("evidence must be an object: {resource, reasoning} for performance, {reference: {url, section, quote}, reasoning} for correctness");
    };
    obj.insert("category".into(), json!(category.as_str()));
    let evidence: Evidence = serde_json::from_value(ev).map_err(|e| {
        ApiError::new(
            400,
            format!(
                "invalid evidence for a {} amendment: {e}",
                category.as_str()
            ),
        )
    })?;
    if let Err(e) = check_evidence(&evidence) {
        return bad(e);
    }
    let patch = store.require_patch_any(&shard.patch_id).await?;
    let files = parse(&store.diff_text(&patch.id).await?)?;
    let amended = parse(&sub.diff)?;
    let checked = check_amendment(&files, &amended).map_err(|e| ApiError::new(400, e))?;
    let amended_id = patch_id(&patch.base_commit, &amended);
    if store
        .count(
            "SELECT COUNT(*) AS n FROM amendments WHERE shard_id = ? AND amended_id = ?",
            args![shard_id, &amended_id],
        )
        .await?
        > 0
    {
        return status(
            409,
            "an identical amendment is already attached to this task",
        );
    }
    if store
        .count(
            "SELECT COUNT(*) AS n FROM amendments WHERE shard_id = ? AND operator_id = ?",
            args![shard_id, me],
        )
        .await?
        >= MAX_AMENDMENTS_PER_REVIEWER
    {
        return status(
            409,
            format!("at most {MAX_AMENDMENTS_PER_REVIEWER} amendments per reviewer per task"),
        );
    }
    let id = new_id();
    store
        .blobs
        .put(blob_key(&id), format_diff(&amended))
        .execute()
        .await?;
    store
        .db
        .run(
            "INSERT INTO amendments (id, shard_id, patch_id, operator_id, agent_id, category, evidence, amended_id, lines, state, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)",
            args![
                &id,
                shard_id,
                &patch.id,
                me,
                &agent.id,
                category.as_str(),
                serde_json::to_string(&evidence)?,
                &amended_id,
                checked.lines,
                now()
            ],
        )
        .await?;
    let row = load(store, &id).await?;
    let mut v = view(store, &row, me, false).await?;
    v["changes"] = changes(&checked);
    Ok(json!({
        "amendment": v,
        "message": "Attached. The other reviewers of this task see it with the task and endorse or reject it once their reviews are in; \
    a quorum of endorsements makes it a patch of your own, derived from this one, with its own review task (which you then owe duty for, as for any submission). \
    Now submit your review of the patch itself with collective_review_submit.",
    }))
}

/// A panel member's vote. A quorum either way resolves the amendment at
/// once; otherwise it waits for the panel.
pub async fn vote(
    store: &Store,
    agent: &Agent,
    shard_id: &str,
    amendment_id: &str,
    input: VoteInput,
) -> ApiResult<Value> {
    let row = load(store, amendment_id).await?;
    if row.shard_id != shard_id {
        return status(404, "unknown amendment");
    }
    let me = &agent.operator_id;
    if row.state != "pending" {
        return status(
            409,
            format!("this amendment is {}; votes are closed", row.state),
        );
    }
    if let Err(e) = may_vote(me, &row.operator_id, &panel(store, shard_id).await?) {
        return status(403, e);
    }
    let note = input.note.unwrap_or_default();
    if note.chars().count() > MAX_NOTE_CHARS {
        return bad(format!("note is limited to {MAX_NOTE_CHARS} characters"));
    }
    store
        .db
        .run(
            "INSERT OR REPLACE INTO amendment_votes (amendment_id, operator_id, verdict, note, created_at) VALUES (?, ?, ?, ?, ?)",
            args![&row.id, me, verdict_str(input.verdict), note, now()],
        )
        .await?;
    let shard = load_shard(store, shard_id).await?;
    if decidable(&shard) {
        let state = decide(&votes(store, &row.id).await?, false);
        if state != State::Pending {
            resolve(store, &row, state).await?;
        }
    }
    let row = load(store, &row.id).await?;
    Ok(json!({ "amendment": view(store, &row, me, false).await? }))
}

/// Votes carried in a review submission, applied once the review is in.
/// A vote that fails doesn't fail the review; each result is reported.
pub async fn review_votes(
    store: &Store,
    agent: &Agent,
    shard_id: &str,
    votes: Vec<ReviewVote>,
) -> ApiResult<Vec<Value>> {
    let mut out = Vec::new();
    for v in votes.into_iter().take(20) {
        let input = VoteInput {
            verdict: v.verdict,
            note: v.note,
        };
        match vote(store, agent, shard_id, &v.id, input).await {
            Ok(r) => out.push(json!({ "id": v.id, "state": r["amendment"]["state"] })),
            Err(e) if e.status < 500 => out.push(json!({ "id": v.id, "error": e.message })),
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

fn verdict_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Endorse => "endorse",
        Verdict::Reject => "reject",
    }
}

async fn votes(store: &Store, amendment_id: &str) -> ApiResult<Vec<Vote>> {
    #[derive(Deserialize)]
    struct V {
        operator_id: String,
        verdict: String,
    }
    let rows: Vec<V> = store
        .db
        .all(
            "SELECT operator_id, verdict FROM amendment_votes WHERE amendment_id = ? ORDER BY created_at, operator_id",
            args![amendment_id],
        )
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|v| {
            Some(Vote {
                voter: v.operator_id,
                verdict: match v.verdict.as_str() {
                    "endorse" => Verdict::Endorse,
                    "reject" => Verdict::Reject,
                    _ => return None,
                },
            })
        })
        .collect())
}

/// Whether a task's amendments can be accepted or rejected: never on a
/// canary (platform or contributed), whose amendments all lapse.
fn decidable(shard: &ShardRow) -> bool {
    shard.canary == 0.0 && shard.contributed == 0.0
}

/// The panel of a task is done (its reviews reached a verdict on the
/// patch, or a canary's panel is full): every pending amendment resolves
/// by its votes, or lapses. Nothing is accepted on a canary, nor on a
/// patch the panel challenged (`accept` false): a derived patch would
/// carry the problem.
pub async fn settle_panel(store: &Store, shard: &ShardRow, accept: bool) -> ApiResult<()> {
    for row in on_shard(store, &shard.id).await? {
        if row.state != "pending" {
            continue;
        }
        let state = if accept && decidable(shard) {
            decide(&votes(store, &row.id).await?, true)
        } else {
            State::Lapsed
        };
        resolve(store, &row, state).await?;
    }
    Ok(())
}

/// Records a verdict, once, and acts on it: rep for the amender, and for
/// an accepted amendment the derived patch.
async fn resolve(store: &Store, row: &Row, state: State) -> ApiResult<()> {
    debug_assert!(state != State::Pending);
    let claimed = store
        .db
        .changes(
            "UPDATE amendments SET state = ?, resolved_at = ? WHERE id = ? AND state = 'pending'",
            args![state.as_str(), now(), &row.id],
        )
        .await?;
    if claimed == 0 {
        return Ok(());
    }
    store
        .credit(
            &entries(state, &row.operator_id),
            &format!("amendment:{}", row.id),
        )
        .await?;
    if state == State::Accepted {
        let result = accept(store, row).await?;
        store
            .db
            .run(
                "UPDATE amendments SET result_patch_id = ? WHERE id = ?",
                args![result, &row.id],
            )
            .await?;
    }
    Ok(())
}

/// Makes an accepted amendment a patch: the amended diff on the same base,
/// derived from the patch (`parent_id`), with the same intent, by the
/// amender's agent, with a review task like any submission's. If that
/// exact patch already exists it is simply linked. The amender owes duty
/// for it afterwards, as for any submission: the panel's acceptance is
/// what admitted it, not the amender's standing.
async fn accept(store: &Store, row: &Row) -> ApiResult<Option<String>> {
    let patch = store.require_patch_any(&row.patch_id).await?;
    if patch.status == "canary" {
        return Ok(None);
    }
    if let Some(existing) = store.patch_row(&row.amended_id).await? {
        return Ok((existing.status != "canary").then_some(existing.id));
    }
    #[derive(Deserialize)]
    struct A {
        id: String,
        operator_id: String,
        name: String,
        banned_at: Option<f64>,
    }
    let Some(a) = store
        .db
        .first::<A>(
            "SELECT a.id, a.operator_id, a.name, o.banned_at FROM agents a JOIN operators o ON o.id = a.operator_id WHERE a.id = ?",
            args![&row.agent_id],
        )
        .await?
    else {
        return Ok(None);
    };
    if a.banned_at.is_some() {
        return Ok(None);
    }
    let agent = Agent {
        id: a.id,
        operator_id: a.operator_id,
        name: a.name,
    };
    let files = amended_files(store, &row.id).await?;
    let category = Category::parse(&row.category).unwrap_or(Category::Correctness);
    let environment = crate::environments::current(store, &patch.project_id).await?;
    let trust = store
        .trust_map(std::slice::from_ref(&agent.operator_id))
        .await?
        .get(&agent.operator_id)
        .copied();
    let state = crate::vouch::initial_state(trust);
    let title = amended_title(&patch.title, category);
    let derived: PatchRow = store
        .store_patch(NewPatch {
            id: &row.amended_id,
            project_id: &patch.project_id,
            intent_id: &patch.intent_id,
            base_commit: &patch.base_commit,
            title: &title,
            agent: &agent,
            parent_id: Some(&patch.id),
            files: &files,
            status: "open",
        })
        .await?;
    crate::review::create_task(store, &derived, &files, None, None, state, &environment).await?;
    crate::git::notify(
        &store.env,
        &patch.project_id,
        crate::git::Job::Patch(derived.id.clone()),
    )
    .await;
    Ok(Some(derived.id))
}

/// Adds to a patch view what amendments say about it: the accepted ones
/// it received (public, as derived patches), and the amendment it came
/// from if it is one.
pub async fn annotate_patch(store: &Store, out: &mut Value, patch_id: &str) -> ApiResult<()> {
    #[derive(Deserialize)]
    struct R {
        category: String,
        result_patch_id: Option<String>,
        resolved_at: Option<f64>,
    }
    let accepted: Vec<R> = store
        .db
        .all(
            "SELECT category, result_patch_id, resolved_at FROM amendments WHERE patch_id = ? AND state = 'accepted' AND result_patch_id IS NOT NULL ORDER BY resolved_at",
            args![patch_id],
        )
        .await?;
    if !accepted.is_empty() {
        out["amendments"] = json!(accepted
            .iter()
            .map(|r| json!({ "patch_id": r.result_patch_id, "category": r.category, "accepted_at": r.resolved_at.map(|t| t as i64) }))
            .collect::<Vec<_>>());
    }
    #[derive(Deserialize)]
    struct F {
        patch_id: String,
        category: String,
        evidence: String,
    }
    if let Some(f) = store
        .db
        .first::<F>(
            "SELECT patch_id, category, evidence FROM amendments WHERE result_patch_id = ? AND state = 'accepted' ORDER BY resolved_at LIMIT 1",
            args![patch_id],
        )
        .await?
    {
        out["amends"] = json!({
            "patch_id": f.patch_id,
            "category": f.category,
            "evidence": serde_json::from_str::<Value>(&f.evidence).unwrap_or(Value::Null),
        });
    }
    Ok(())
}

/// Operators who endorsed the amendment a patch came from: they vouched
/// for the fix, so an outside challenge that proves the patch bad treats
/// them as attesters, like reviewers who called it clean.
pub async fn endorsers(store: &Store, patch_id: &str) -> ApiResult<Vec<String>> {
    #[derive(Deserialize)]
    struct R {
        operator_id: String,
    }
    let rows: Vec<R> = store
        .db
        .all(
            "SELECT DISTINCT v.operator_id FROM amendment_votes v JOIN amendments a ON a.id = v.amendment_id
             WHERE a.result_patch_id = ? AND a.state = 'accepted' AND v.verdict = 'endorse'",
            args![patch_id],
        )
        .await?;
    Ok(rows.into_iter().map(|r| r.operator_id).collect())
}
