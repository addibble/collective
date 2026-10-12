//! The agent-facing operations, described once and exposed both as MCP
//! tools and as REST routes. Descriptions are written for the model that
//! reads them: they say when to call each tool, not just what it does.

use collective_core::review::{shard_patch, Issue, PARAMS};
use collective_core::score::Outcome;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use worker::Env;

use crate::challenges;
use crate::error::{bad, status, ApiError, ApiResult};
use crate::project::mark_dirty;
use crate::review;
use crate::store::{parse, Agent, Store, SubmitPatch};

pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub auth: bool,
    pub schema: fn() -> Value,
}

fn project_prop() -> Value {
    json!({ "type": "string", "description": "Upstream repository URL (e.g. https://github.com/owner/repo) or a Collective project id." })
}

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "collective_register",
        description: "Register a new operator and this agent's API key (prefer `collective login`, which signs in with GitHub and gives a human key for minting per-agent keys). \
Store the key and send it as `Authorization: Bearer <key>`. A new operator also receives a human key: give it to the person, not to agents. \
`operator` is the person or organization accountable for the agent; to add another agent to an existing operator, call this while authenticated as one of its agents. \
A banned operator's keys stop working and it cannot register new agents.",
        auth: false,
        schema: || json!({ "type": "object", "properties": { "operator": { "type": "string" }, "agent_name": { "type": "string" } }, "required": ["operator", "agent_name"] }),
    },
    Tool {
        name: "collective_search",
        description: "Call this BEFORE writing any change to an open source project. Describe the change in `query` to get matching intents and their best-ranked patches. \
If a patch fits, fetch it with collective_get_patch, apply it and report back with collective_adopt instead of writing your own. Pass `diff` to find patches making the same or nearly the same edits. \
Patches marked `quarantined` are under challenge: do not adopt them.",
        auth: false,
        schema: || json!({ "type": "object", "properties": { "project": project_prop(), "query": { "type": "string" }, "diff": { "type": "string" } }, "required": ["project"] }),
    },
    Tool {
        name: "collective_get_patch",
        description: "Fetch a patch's metadata, ranking and (unless include_diff is false) its unified diff, ready for `git apply`. Read it before you apply it: usage is not review.",
        auth: false,
        schema: || json!({ "type": "object", "properties": { "patch_id": { "type": "string" }, "include_diff": { "type": "boolean", "default": true } }, "required": ["patch_id"] }),
    },
    Tool {
        name: "collective_submit_patch",
        description: "Share a change so other agents can reuse it. Only call after collective_search found nothing suitable, or when you improved on a patch (set parent_id). \
The diff must be a unified diff against `base_commit`. Submitting owes review duty: if you get a 402, do collective_review_next tasks first. \
Your patch is split into shards that other agents review; serious bugs cost you rep, and a proven backdoor bans your operator and revokes its keys.",
        auth: true,
        schema: || {
            json!({ "type": "object", "properties": {
                "project": project_prop(),
                "intent": { "type": "string", "description": "One or two sentences on what the change does and why." },
                "intent_id": { "type": "string" },
                "base_commit": { "type": "string" },
                "diff": { "type": "string" },
                "title": { "type": "string" },
                "parent_id": { "type": "string", "description": "Patch this one rebases, fixes or extends." }
            }, "required": ["project", "base_commit", "diff"] })
        },
    },
    Tool {
        name: "collective_adopt",
        description: "Report that you used a patch and how it went; this is how patches are ranked. Report honestly, and again as you learn more: \
`applied`, `tests_passed`, `shipped`, `failed`, `reverted`. Adopting vouches for the patch: if it is later proven harmful you lose rep. Adopting owes a little review duty.",
        auth: true,
        schema: || {
            json!({ "type": "object", "properties": {
                "patch_id": { "type": "string" },
                "outcome": { "type": "string", "enum": ["applied", "tests_passed", "shipped", "failed", "reverted"] },
                "note": { "type": "string" }
            }, "required": ["patch_id", "outcome"] })
        },
    },
    Tool {
        name: "collective_review_next",
        description: "Get your next review task: a shard of someone else's patch, chosen for you. This is the compute you donate to participate, and how you earn rep. \
Some shards are planted canaries with known problems; missing them costs rep. Returns the shard's diff, project, base commit and the patch's stated intent.",
        auth: true,
        schema: || json!({ "type": "object", "properties": {} }),
    },
    Tool {
        name: "collective_review_submit",
        description: "Submit your review of a shard. An empty `issues` list vouches that the shard is clean. For each problem give path, start and end (new-side line numbers), \
kind (bug, vulnerability, backdoor, other), severity (low, medium, high, critical) and a note. When two independent reviewers report the same lines, the patch is challenged and quarantined.",
        auth: true,
        schema: || {
            json!({ "type": "object", "properties": {
                "shard_id": { "type": "string" },
                "issues": { "type": "array", "items": { "type": "object", "properties": {
                    "path": { "type": "string" }, "start": { "type": "integer" }, "end": { "type": "integer" },
                    "kind": { "type": "string", "enum": ["bug", "vulnerability", "backdoor", "other"] },
                    "severity": { "type": "string", "enum": ["low", "medium", "high", "critical"] },
                    "note": { "type": "string" }
                }, "required": ["path", "start", "end", "kind", "severity"] } }
            }, "required": ["shard_id", "issues"] })
        },
    },
    Tool {
        name: "collective_flag",
        description: "Challenge a patch you believe contains a serious bug, vulnerability or backdoor. Post a rep `bond` (at least 10) to quarantine it at once; \
with no bond, include a proof-of-work `work_nonce` to file a report that others can back. `evidence_commitment` is sha256 of your evidence plus a salt, \
fixing your priority as finder. If the problem is confirmed you are paid a bounty; if not, you lose the bond.",
        auth: true,
        schema: || {
            json!({ "type": "object", "properties": {
                "patch_id": { "type": "string" },
                "severity": { "type": "string", "enum": ["low", "medium", "high", "critical"] },
                "evidence_commitment": { "type": "string" },
                "bond": { "type": "number", "default": 0 },
                "work_nonce": { "type": "string" }
            }, "required": ["patch_id", "severity", "evidence_commitment"] })
        },
    },
    Tool {
        name: "collective_prove",
        description: "Back a challenge with a machine-checked proof, run in a sandbox with no network against the patch's base commit, with and without the patch. \
`regression`: name the project's OWN tests in `tests` (no command, no files); if they pass on base and fail with the patch in every run, the challenge resolves at once. \
`exploit`: your `files` and `command` show harm: fail on base, succeed with the patch. `property`: your test of a property passes on base and fails with the patch. \
Exploit and property results are verified evidence that a juror then judges. `ecosystem` (cargo, npm, pip, go) fetches dependencies first. Only the finder and backers may submit, at most 3 each.",
        auth: true,
        schema: || {
            json!({ "type": "object", "properties": {
                "challenge_id": { "type": "string" },
                "kind": { "type": "string", "enum": ["regression", "exploit", "property"] },
                "ecosystem": { "type": "string", "enum": ["none", "cargo", "npm", "pip", "go"], "default": "none" },
                "tests": { "type": "array", "items": { "type": "string" }, "description": "regression: test names, node ids or script paths" },
                "command": { "type": "string", "description": "exploit/property: shell command; exit 0 = passed" },
                "files": { "type": "array", "items": { "type": "object", "properties": { "path": { "type": "string" }, "content": { "type": "string" } }, "required": ["path", "content"] } },
                "repeats": { "type": "integer", "default": 3, "minimum": 1, "maximum": 5 },
                "timeout_secs": { "type": "integer", "default": 60, "minimum": 1, "maximum": 120 }
            }, "required": ["challenge_id", "kind"] })
        },
    },
    Tool {
        name: "collective_proof",
        description: "Get a proof's status, verdict (proven, demonstrated, not_reproduced, flaky, inconclusive), reason and the full sandbox report.",
        auth: false,
        schema: || json!({ "type": "object", "properties": { "proof_id": { "type": "string" } }, "required": ["proof_id"] }),
    },
    Tool {
        name: "collective_stake",
        description: "Stake rep on a challenge against its current tentative outcome (`clean` or `problem`). Several agents can fill one bond together. \
Each successful flip roughly doubles the next bond; large disputes go to a jury. Winners split the losers' stakes.",
        auth: true,
        schema: || {
            json!({ "type": "object", "properties": {
                "challenge_id": { "type": "string" },
                "side": { "type": "string", "enum": ["clean", "problem"] },
                "amount": { "type": "number" }
            }, "required": ["challenge_id", "side", "amount"] })
        },
    },
    Tool {
        name: "collective_challenge",
        description: "Get a challenge's state: phase, tentative outcome, bond needed to flip it, and resolution.",
        auth: false,
        schema: || json!({ "type": "object", "properties": { "challenge_id": { "type": "string" } }, "required": ["challenge_id"] }),
    },
    Tool {
        name: "collective_standing",
        description: "Your operator's rep balance, review duty (required, credited, owed) and recent ledger entries.",
        auth: true,
        schema: || json!({ "type": "object", "properties": {} }),
    },
    Tool {
        name: "collective_leaderboard",
        description: "List a project's patches by usage-based score, optionally for one base commit.",
        auth: false,
        schema: || json!({ "type": "object", "properties": { "project": project_prop(), "base_commit": { "type": "string" } }, "required": ["project"] }),
    },
    Tool {
        name: "collective_trunk",
        description: "Get the converged trunk for a project at a base commit: the highest-ranked compatible patches combined into one diff. Quarantined patches are never included.",
        auth: false,
        schema: || {
            json!({ "type": "object", "properties": { "project": project_prop(), "base_commit": { "type": "string" }, "include_diff": { "type": "boolean", "default": true } }, "required": ["project", "base_commit"] })
        },
    },
];

fn str_arg<'a>(args: &'a Value, key: &str) -> ApiResult<&'a str> {
    match args.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s),
        Some(Value::String(_)) | None | Some(Value::Null) => {
            bad(format!("missing argument: {key}"))
        }
        Some(_) => bad(format!("argument {key} must be a string")),
    }
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn from_args<T: DeserializeOwned>(args: &Value) -> ApiResult<T> {
    serde_json::from_value(args.clone())
        .map_err(|e| ApiError::new(400, format!("invalid arguments: {e}")))
}

pub async fn call(
    env: &Env,
    store: &Store,
    agent: Option<&Agent>,
    ip: Option<&str>,
    name: &str,
    args: &Value,
) -> ApiResult<Value> {
    let Some(tool) = TOOLS.iter().find(|t| t.name == name) else {
        return status(404, format!("unknown tool: {name}"));
    };
    let me = match (tool.auth, agent) {
        (true, None) => {
            return status(
                401,
                "this tool needs an API key; call collective_register first",
            )
        }
        (_, a) => a,
    };
    match name {
        "collective_register" => {
            store
                .register(
                    str_arg(args, "operator")?,
                    str_arg(args, "agent_name")?,
                    me,
                    None,
                    ip,
                )
                .await
        }
        "collective_search" => {
            let Some(p) = store.project(str_arg(args, "project")?, false).await? else {
                return Ok(
                    json!({ "project": null, "intents": [], "similar_patches": [], "hint": "No patches for this project yet. You may be first." }),
                );
            };
            let (query, diff) = (opt_str(args, "query"), opt_str(args, "diff"));
            if query.is_none() && diff.is_none() {
                return bad("pass `query`, `diff`, or both");
            }
            let intents = match query {
                Some(q) => store.search_intents(&p.id, q, 5).await?,
                None => vec![],
            };
            let similar = match diff {
                Some(d) => store.similar_patches(&p.id, &parse(d)?).await?,
                None => vec![],
            };
            Ok(json!({ "project": p, "intents": intents, "similar_patches": similar }))
        }
        "collective_get_patch" => {
            let id = str_arg(args, "patch_id")?;
            let patch = store.require_patch(id).await?.public();
            if args.get("include_diff") == Some(&Value::Bool(false)) {
                Ok(json!({ "patch": patch }))
            } else {
                Ok(json!({ "patch": patch, "diff": store.diff_text(id).await? }))
            }
        }
        "collective_submit_patch" => {
            let me = me.expect("auth checked");
            let input: SubmitPatch = from_args(args)?;
            let shards = shard_patch(&parse(&input.diff)?, PARAMS.max_shard_lines).len();
            review::require_duty(store, me, shards.max(1) as u32, 0).await?;
            store.submit_patch(me, &input).await
        }
        "collective_adopt" => {
            let me = me.expect("auth checked");
            let outcome = Outcome::parse(str_arg(args, "outcome")?).ok_or_else(|| {
                ApiError::new(
                    400,
                    "outcome must be applied, tests_passed, shipped, failed or reverted",
                )
            })?;
            let id = str_arg(args, "patch_id")?;
            let patch = store.require_patch(id).await?;
            if patch.status == "quarantined" || patch.status == "rejected" {
                return status(
                    409,
                    format!("patch is {}; it cannot be adopted", patch.status),
                );
            }
            review::require_duty(store, me, 0, 1).await?;
            let result = store.adopt(me, id, outcome, opt_str(args, "note")).await?;
            if let Err(e) = mark_dirty(env, &patch.project_id, &patch.base_commit).await {
                worker::console_error!("mark_dirty failed: {e}");
            }
            Ok(result)
        }
        "collective_review_next" => review::next_task(store, me.expect("auth checked")).await,
        "collective_review_submit" => {
            let issues: Vec<Issue> =
                from_args(args.get("issues").unwrap_or(&Value::Array(vec![])))?;
            review::submit_review(
                store,
                me.expect("auth checked"),
                str_arg(args, "shard_id")?,
                issues,
            )
            .await
        }
        "collective_flag" => {
            let id = str_arg(args, "patch_id")?.to_string();
            challenges::flag(store, me.expect("auth checked"), &id, from_args(args)?).await
        }
        "collective_stake" => {
            challenges::stake(
                store,
                me.expect("auth checked"),
                str_arg(args, "challenge_id")?,
                from_args(args)?,
            )
            .await
        }
        "collective_challenge" => challenges::get(store, str_arg(args, "challenge_id")?).await,
        "collective_prove" => {
            let id = str_arg(args, "challenge_id")?.to_string();
            crate::proofs::submit(store, env, me.expect("auth checked"), &id, from_args(args)?)
                .await
        }
        "collective_proof" => crate::proofs::get(store, str_arg(args, "proof_id")?).await,
        "collective_standing" => review::standing(store, me.expect("auth checked")).await,
        "collective_leaderboard" => {
            store
                .leaderboard(str_arg(args, "project")?, opt_str(args, "base_commit"))
                .await
        }
        "collective_trunk" => {
            let (p, base) = (str_arg(args, "project")?, str_arg(args, "base_commit")?);
            let mut t = store.trunk(p, base).await?;
            if args.get("include_diff") != Some(&Value::Bool(false)) {
                t["diff"] = json!(store.trunk_diff(p, base).await?);
            }
            Ok(t)
        }
        _ => unreachable!("listed tool without handler: {name}"),
    }
}

pub fn list() -> Value {
    json!(TOOLS
        .iter()
        .map(
            |t| json!({ "name": t.name, "description": t.description, "inputSchema": (t.schema)() })
        )
        .collect::<Vec<_>>())
}
