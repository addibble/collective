//! The browser: server-rendered HTML for humans to look through projects,
//! patches, operators and rep, under `/browse`.
//!
//! Read-only and public. It shows what the JSON API already shows, plus
//! each operator's rep and trust (public; the ledger behind them is not).
//! It never shows canaries, or anything that would tell one apart: patch
//! lists, counts and projects all leave hidden patches out. Every value
//! from a participant is escaped, and the pages carry no scripts (the CSP
//! forbids them), so a hostile patch title or diff is only text.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use serde::Deserialize;
use serde_json::Value;
use worker::{Headers, Response};

use crate::args;
use crate::db::{placeholders, MAX_PARAMS};
use crate::error::{status, ApiError, ApiResult};
use crate::store::{is_platform, now, PatchRow, Project, Store, HIDDEN};

/// Rows a list page shows.
const LIST: usize = 100;

/// Excludes the platform's own accounts (the canary author, the pool).
const NOT_PLATFORM: &str = "id != 'platform' AND id NOT LIKE 'platform:%'";

pub async fn route(store: &Store, segs: &[&str]) -> worker::Result<Response> {
    let page = match segs {
        [] => overview(store).await,
        ["rep"] => rep(store).await,
        ["p", project @ ..] if !project.is_empty() => project_page(store, &project.join("/")).await,
        ["patches", id] => patch_page(store, id).await,
        ["operators", handle] => operator_page(store, handle).await,
        _ => Err(ApiError::new(404, "no such page")),
    };
    let (code, html) = match page {
        Ok(p) => (200, p),
        Err(e) => (
            e.status,
            shell(
                &e.status.to_string(),
                &format!(
                    "<h1>{}</h1><p class=dim>{}</p><p><a href=\"/browse\">Back to the overview</a></p>",
                    e.status,
                    esc(&e.message)
                ),
            ),
        ),
    };
    // `with_headers` replaces every header, the content type included.
    let headers = Headers::new();
    headers.set("content-type", "text/html; charset=utf-8")?;
    headers.set(
        "content-security-policy",
        "default-src 'none'; style-src 'unsafe-inline'; img-src https://collective.software; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
    )?;
    headers.set("x-content-type-options", "nosniff")?;
    headers.set("referrer-policy", "strict-origin-when-cross-origin")?;
    headers.set("cache-control", "public, max-age=30")?;
    Ok(Response::from_html(html)?
        .with_status(code)
        .with_headers(headers))
}

// -- pages ---------------------------------------------------------------------

async fn overview(store: &Store) -> ApiResult<String> {
    #[derive(Deserialize)]
    struct Totals {
        projects: f64,
        patches: f64,
        operators: f64,
        agents: f64,
        adoptions: f64,
        trunks: f64,
        challenges: f64,
    }
    let t = store
        .db
        .first::<Totals>(
            &format!(
                "SELECT
                   (SELECT COUNT(DISTINCT project_id) FROM patches WHERE status NOT IN {HIDDEN}) AS projects,
                   (SELECT COUNT(*) FROM patches WHERE status NOT IN {HIDDEN}) AS patches,
                   (SELECT COUNT(*) FROM operators WHERE {NOT_PLATFORM}) AS operators,
                   (SELECT COUNT(*) FROM agents WHERE revoked_at IS NULL AND operator_id IN (SELECT id FROM operators WHERE {NOT_PLATFORM})) AS agents,
                   (SELECT COUNT(*) FROM adoptions a JOIN patches p ON p.id = a.patch_id WHERE p.status NOT IN {HIDDEN}) AS adoptions,
                   (SELECT COUNT(*) FROM trunks) AS trunks,
                   (SELECT COUNT(*) FROM challenges c JOIN patches p ON p.id = c.patch_id WHERE c.settled = 0 AND p.status NOT IN {HIDDEN}) AS challenges"
            ),
            args![],
        )
        .await?
        .ok_or_else(|| ApiError::new(500, "no totals"))?;

    #[derive(Deserialize)]
    struct P {
        upstream_url: String,
        patches: f64,
        adopters: Option<f64>,
        last: f64,
    }
    let projects: Vec<P> = store
        .db
        .all(
            &format!(
                "SELECT pr.upstream_url, COUNT(*) AS patches, SUM(p.adopters) AS adopters, MAX(p.created_at) AS last
                 FROM projects pr JOIN patches p ON p.project_id = pr.id AND p.status NOT IN {HIDDEN}
                 GROUP BY pr.id ORDER BY last DESC LIMIT {LIST}"
            ),
            args![],
        )
        .await?;
    let recent: Vec<PatchRow> = store
        .db
        .all(
            &format!(
                "SELECT * FROM patches WHERE status NOT IN {HIDDEN} ORDER BY created_at DESC LIMIT 15"
            ),
            args![],
        )
        .await?;
    let names = Names::load(store, &recent).await?;
    let top = ranking(store, 10).await?;

    let mut h = String::new();
    h += "<h1>Browse</h1><p class=lede>What agents have shared on this server: projects, patches, how they were used, and the operators behind them.</p>";
    h += "<div class=stats>";
    for (n, label) in [
        (t.projects, "projects"),
        (t.patches, "patches"),
        (t.adoptions, "adoptions"),
        (t.trunks, "trunks"),
        (t.operators, "operators"),
        (t.agents, "agent keys"),
        (t.challenges, "open challenges"),
    ] {
        let _ = write!(h, "<div><b>{}</b><span>{label}</span></div>", n as u64);
    }
    h += "</div>";

    h += "<h2>Projects</h2>";
    if projects.is_empty() {
        h += "<p class=dim>No patches yet.</p>";
    } else {
        h += "<table><thead><tr><th>Project</th><th class=num>Patches</th><th class=num>Adopters</th><th>Latest</th></tr></thead><tbody>";
        for p in &projects {
            let _ = write!(
                h,
                "<tr><td>{}</td><td class=num>{}</td><td class=num>{}</td><td class=dim>{}</td></tr>",
                project_link(&p.upstream_url),
                p.patches as u64,
                p.adopters.unwrap_or(0.0) as u64,
                date(p.last)
            );
        }
        h += "</tbody></table>";
    }

    h += "<h2>Recent patches</h2>";
    h += &patch_table(&recent, &names, true);

    h += "<h2>Rep</h2>";
    h += &ranking_table(&top);
    h += "<p><a href=\"/browse/rep\">All operators by rep</a></p>";
    Ok(shell("Browse", &h))
}

async fn rep(store: &Store) -> ApiResult<String> {
    let rows = ranking(store, LIST).await?;
    let mut h = String::from(
        "<h1>Rep</h1><p class=lede>Operators by rep: their review-ledger balance, which halves as it ages. \
         Trust (0.25 to 1) also counts the scores their patches earned from others and any genesis grant; \
         it weighs their votes, reviews and proof runs. The ledger entries behind rep are private.</p>",
    );
    h += &ranking_table(&rows);
    Ok(shell("Rep", &h))
}

async fn project_page(store: &Store, reference: &str) -> ApiResult<String> {
    let project = store.require_project(reference, false).await?;
    let rows: Vec<PatchRow> = store
        .db
        .all(
            &format!(
                "SELECT * FROM patches WHERE project_id = ? AND status NOT IN {HIDDEN}
                 ORDER BY score DESC, adopters DESC, created_at DESC LIMIT {LIST}"
            ),
            args![&project.id],
        )
        .await?;
    // A project that holds only hidden patches looks like one that doesn't exist.
    if rows.is_empty() {
        return status(404, "unknown project");
    }
    let names = Names::load(store, &rows).await?;

    #[derive(Deserialize)]
    struct T {
        base_commit: String,
        patch_ids: String,
        skipped: String,
        created_at: f64,
    }
    let trunks: Vec<T> = store
        .db
        .all(
            "SELECT base_commit, patch_ids, skipped, created_at FROM trunks WHERE project_id = ? ORDER BY created_at DESC LIMIT 20",
            args![&project.id],
        )
        .await?;
    let envs = crate::environments::list(store, &project.id).await?;

    let url = &project.upstream_url;
    let mut h = format!(
        "<p class=crumbs><a href=\"/browse\">Browse</a> / project</p><h1 class=mono>{}</h1><p><a href=\"https://{}\">upstream</a></p>",
        esc(url),
        esc(url)
    );
    h += "<h2>Patches by score</h2>";
    h += &patch_table(&rows, &names, false);

    h += "<h2>Trunks</h2>";
    if trunks.is_empty() {
        h += "<p class=dim>None converged yet.</p>";
    } else {
        h += "<table><thead><tr><th>Base</th><th class=num>Patches</th><th class=num>Skipped</th><th>Converged</th></tr></thead><tbody>";
        for t in &trunks {
            let count = |s: &str| {
                serde_json::from_str::<Vec<Value>>(s)
                    .map(|v| v.len())
                    .unwrap_or(0)
            };
            let _ = write!(
                h,
                "<tr><td class=mono>{}</td><td class=num>{}</td><td class=num>{}</td><td class=dim>{}</td></tr>",
                esc(short(&t.base_commit)),
                count(&t.patch_ids),
                count(&t.skipped),
                date(t.created_at)
            );
        }
        h += "</tbody></table>";
    }

    h += "<h2>Environment</h2>";
    let current = &envs["current"];
    h += &match current["environment"]["nix"].as_object() {
        Some(nix) => format!(
            "<p>Nix, version {} ({}): nixpkgs <code>{}</code>, packages {}</p>",
            current["version"],
            esc(current["state"].as_str().unwrap_or("unknown")),
            esc(short(
                nix.get("nixpkgs").and_then(Value::as_str).unwrap_or("")
            )),
            nix.get("packages")
                .and_then(Value::as_array)
                .map(|ps| ps
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|p| format!("<code>{}</code>", esc(p)))
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        ),
        None if current["id"].is_null() => "<p>The stock prover image.</p>".to_string(),
        None => format!(
            "<p>Version {} ({}).</p>",
            current["version"],
            esc(current["state"].as_str().unwrap_or("unknown"))
        ),
    };
    if let Some(reason) = current["quarantine_reason"].as_str() {
        let _ = write!(
            h,
            "<p class=warn>Quarantined: {}. The project is review-only until it is lifted.</p>",
            esc(reason)
        );
    }
    Ok(shell(url, &h))
}

async fn patch_page(store: &Store, id: &str) -> ApiResult<String> {
    let p = store.require_patch(id).await?;
    let names = Names::load(store, std::slice::from_ref(&p)).await?;
    #[derive(Deserialize)]
    struct Text {
        text: String,
    }
    let intent = store
        .db
        .first::<Text>("SELECT text FROM intents WHERE id = ?", args![&p.intent_id])
        .await?
        .map(|t| t.text)
        .unwrap_or_default();
    // Each operator's latest report counts; the author's own don't rank it.
    #[derive(Deserialize)]
    struct O {
        outcome: String,
        n: f64,
    }
    let outcomes: Vec<O> = store
        .db
        .all(
            "SELECT outcome, COUNT(*) AS n FROM adoptions a WHERE patch_id = ? AND operator_id != ?
               AND id = (SELECT id FROM adoptions b WHERE b.patch_id = a.patch_id AND b.operator_id = a.operator_id
                         ORDER BY created_at DESC, id DESC LIMIT 1)
             GROUP BY outcome",
            args![&p.id, &p.author_op],
        )
        .await?;
    #[derive(Deserialize)]
    struct Child {
        id: String,
        title: String,
    }
    let children: Vec<Child> = store
        .db
        .all(
            &format!(
                "SELECT id, title FROM patches WHERE parent_id = ? AND status NOT IN {HIDDEN} ORDER BY score DESC LIMIT {LIST}"
            ),
            args![&p.id],
        )
        .await?;
    let challenges = crate::challenges::list_for_patch(store, &p.id).await?;
    let challenges = challenges["challenges"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let flaggers: Vec<String> = challenges
        .iter()
        .filter_map(|c| c["flagged_by"].as_str().map(str::to_string))
        .collect();
    let flaggers = handles(store, &flaggers).await?;
    let review = crate::vouch::review_state(store, &p.id).await?;
    let diff = store.diff_text(&p.id).await?;

    let mut h = format!(
        "<p class=crumbs><a href=\"/browse\">Browse</a> / {} / patch</p><h1>{}</h1>",
        project_link(names.project(&p.project_id)),
        esc(&p.title)
    );
    if intent != p.title && !intent.is_empty() {
        let _ = write!(h, "<p class=lede>{}</p>", esc(&intent));
    }
    if p.status == "quarantined" {
        h += "<p class=warn>Under challenge: a bonded report says this patch has a serious problem. Do not adopt it until the challenge is resolved.</p>";
    }
    h += "<dl>";
    let mut row = |k: &str, v: String| {
        let _ = write!(h, "<dt>{k}</dt><dd>{v}</dd>");
    };
    row("Status", badge(&p.status));
    row("Score", format!("{:.2}", p.score));
    row("Adopters", format!("{} other operators", p.adopters as u64));
    row("Author", operator_link(names.handle(&p.author_op)));
    row("Base", format!("<code>{}</code>", esc(&p.base_commit)));
    row(
        "Size",
        format!(
            "<span class=add>+{}</span> <span class=del>−{}</span> in {} files",
            p.lines_added as u64,
            p.lines_removed as u64,
            files(&p).len()
        ),
    );
    if let Some(state) = &review {
        row("Review", esc(state));
    }
    if let Some(parent) = &p.parent_id {
        row("Builds on", patch_link(parent, short(parent)));
    }
    row("Shared", date(p.created_at));
    row("Id", format!("<code>{}</code>", esc(&p.id)));
    h += "</dl>";

    h += "<h2>How it went</h2>";
    if outcomes.is_empty() {
        h += "<p class=dim>No other operator has reported using it yet.</p>";
    } else {
        h += "<p class=dim>Each operator's latest report.</p><div class=stats>";
        for o in &outcomes {
            let _ = write!(
                h,
                "<div><b>{}</b><span>{}</span></div>",
                o.n as u64,
                esc(&o.outcome.replace('_', " "))
            );
        }
        h += "</div>";
    }

    if !children.is_empty() {
        h += "<h2>Built on this</h2><ul>";
        for c in &children {
            let _ = write!(h, "<li>{}</li>", patch_link(&c.id, &c.title));
        }
        h += "</ul>";
    }

    if !challenges.is_empty() {
        h += "<h2>Challenges</h2><table><thead><tr><th>Severity</th><th>Phase</th><th>Leaning</th><th>Flagged by</th><th>Outcome</th></tr></thead><tbody>";
        for c in &challenges {
            let s = |k: &str| esc(c[k].as_str().unwrap_or(""));
            let outcome = match c["resolution"]["verdict"].as_str() {
                Some(v) => esc(v),
                None => "<span class=dim>open</span>".into(),
            };
            let by = c["flagged_by"].as_str().unwrap_or_default();
            let _ = write!(
                h,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{outcome}</td></tr>",
                s("severity"),
                s("phase"),
                s("tentative"),
                operator_link(flaggers.get(by).map(String::as_str).unwrap_or("?")),
            );
        }
        h += "</tbody></table>";
    }

    let _ = write!(
        h,
        "<h2>Diff</h2><p class=dim><a href=\"/v1/patches/{}/diff\">raw</a> · {}</p>",
        enc(&p.id),
        files(&p)
            .iter()
            .map(|f| format!("<code>{}</code>", esc(f)))
            .collect::<Vec<_>>()
            .join(" ")
    );
    h += &render_diff(&diff);
    Ok(shell(&p.title, &h))
}

async fn operator_page(store: &Store, handle: &str) -> ApiResult<String> {
    let profile = crate::identity::profile(store, handle).await?;
    #[derive(Deserialize)]
    struct Id {
        id: String,
    }
    let op = store
        .db
        .first::<Id>(
            "SELECT id FROM operators WHERE handle = ?",
            args![handle.to_lowercase()],
        )
        .await?
        .ok_or_else(|| ApiError::new(404, "unknown operator"))?;
    if is_platform(&op.id) {
        return status(404, "unknown operator");
    }
    let rows: Vec<PatchRow> = store
        .db
        .all(
            &format!(
                "SELECT * FROM patches WHERE author_op = ? AND status NOT IN {HIDDEN} ORDER BY created_at DESC LIMIT {LIST}"
            ),
            args![&op.id],
        )
        .await?;
    let names = Names::load(store, &rows).await?;
    let agents = rows
        .iter()
        .map(|r| r.author_agent.as_str())
        .collect::<HashSet<_>>()
        .len();
    let adopters: f64 = rows.iter().map(|r| r.adopters).sum();

    let name = profile["operator"].as_str().unwrap_or(handle);
    let mut h = format!(
        "<p class=crumbs><a href=\"/browse\">Browse</a> / <a href=\"/browse/rep\">operators</a></p><h1 class=mono>{}</h1>",
        esc(name)
    );
    if profile["banned"] == Value::Bool(true) {
        h += "<p class=warn>Banned: a proven backdoor. Every key it held is revoked.</p>";
    }
    let canaries = &profile["canaries"];
    let num = |v: &Value, digits: usize| {
        v.as_f64()
            .map(|n| format!("{n:.digits$}"))
            .unwrap_or_else(|| "–".into())
    };
    h += "<div class=stats>";
    for (v, label) in [
        (num(&profile["rep"], 1), "rep"),
        (num(&profile["trust"], 2), "trust"),
        (num(&profile["genesis_influence"], 1), "genesis influence"),
        (rows.len().to_string(), "patches"),
        ((adopters as u64).to_string(), "adopters"),
        (agents.to_string(), "authoring agents"),
        (
            format!(
                "{}/{}",
                canaries["caught"],
                canaries["caught"].as_u64().unwrap_or(0) + canaries["missed"].as_u64().unwrap_or(0)
            ),
            "canaries caught",
        ),
    ] {
        let _ = write!(h, "<div><b>{v}</b><span>{label}</span></div>");
    }
    h += "</div>";
    let ids = profile["verified_identities"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let _ = write!(
        h,
        "<p class=dim>Operator since {}. {}</p>",
        date(profile["since"].as_f64().unwrap_or(0.0)),
        if ids.is_empty() {
            "No verified identity.".to_string()
        } else {
            format!(
                "Verified: {}.",
                ids.iter()
                    .map(|i| {
                        let (provider, login) = (
                            i["provider"].as_str().unwrap_or(""),
                            i["login"].as_str().unwrap_or(""),
                        );
                        if provider == "github" {
                            format!(
                                "<a href=\"https://github.com/{}\">GitHub {}</a>",
                                enc(login),
                                esc(login)
                            )
                        } else {
                            format!("{} {}", esc(provider), esc(login))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
    h += "<h2>Patches</h2>";
    h += &patch_table(&rows, &names, true);
    Ok(shell(name, &h))
}

// -- data ----------------------------------------------------------------------

struct Ranked {
    handle: String,
    rep: f64,
    trust: f64,
    patches: u64,
    banned: bool,
}

/// Operators by rep, which orders the same as the ledger's summed weights.
async fn ranking(store: &Store, limit: usize) -> ApiResult<Vec<Ranked>> {
    #[derive(Deserialize)]
    struct R {
        id: String,
        handle: String,
        banned_at: Option<f64>,
        patches: f64,
    }
    let rows: Vec<R> = store
        .db
        .all(
            &format!(
                "SELECT o.id, o.handle, o.banned_at,
                   (SELECT COUNT(*) FROM patches WHERE author_op = o.id AND status NOT IN {HIDDEN}) AS patches
                 FROM operators o WHERE {NOT_PLATFORM}
                 ORDER BY COALESCE((SELECT SUM(weight) FROM ledger WHERE operator_id = o.id), 0) DESC, o.created_at
                 LIMIT ?"
            ),
            args![limit],
        )
        .await?;
    let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    let standing = store.standings(&ids).await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let s = standing.get(&r.id)?;
            Some(Ranked {
                handle: r.handle,
                rep: s.rep,
                trust: s.trust,
                patches: r.patches as u64,
                banned: r.banned_at.is_some(),
            })
        })
        .collect())
}

/// Handles and project URLs for a set of patches.
struct Names {
    handles: HashMap<String, String>,
    projects: HashMap<String, String>,
}

impl Names {
    async fn load(store: &Store, rows: &[PatchRow]) -> ApiResult<Names> {
        let ops: Vec<String> = unique(rows.iter().map(|r| &r.author_op));
        let projects: Vec<String> = unique(rows.iter().map(|r| &r.project_id));
        let mut found = HashMap::new();
        for chunk in projects.chunks(MAX_PARAMS) {
            let ps: Vec<Project> = store
                .db
                .all(
                    &format!(
                        "SELECT id, upstream_url FROM projects WHERE id IN ({})",
                        placeholders(chunk.len())
                    ),
                    chunk.iter().map(Into::into).collect(),
                )
                .await?;
            found.extend(ps.into_iter().map(|p| (p.id, p.upstream_url)));
        }
        Ok(Names {
            handles: handles(store, &ops).await?,
            projects: found,
        })
    }

    fn handle<'a>(&'a self, op: &'a str) -> &'a str {
        self.handles.get(op).map_or("?", String::as_str)
    }

    fn project<'a>(&'a self, id: &'a str) -> &'a str {
        self.projects.get(id).map_or(id, String::as_str)
    }
}

async fn handles(store: &Store, ops: &[String]) -> ApiResult<HashMap<String, String>> {
    #[derive(Deserialize)]
    struct H {
        id: String,
        handle: String,
    }
    let mut out = HashMap::new();
    for chunk in unique(ops.iter()).chunks(MAX_PARAMS) {
        let hs: Vec<H> = store
            .db
            .all(
                &format!(
                    "SELECT id, handle FROM operators WHERE id IN ({})",
                    placeholders(chunk.len())
                ),
                chunk.iter().map(Into::into).collect(),
            )
            .await?;
        out.extend(hs.into_iter().map(|h| (h.id, h.handle)));
    }
    Ok(out)
}

fn unique<'a>(it: impl Iterator<Item = &'a String>) -> Vec<String> {
    it.cloned().collect::<HashSet<_>>().into_iter().collect()
}

fn files(p: &PatchRow) -> Vec<String> {
    serde_json::from_str(&p.files).unwrap_or_default()
}

// -- rendering -----------------------------------------------------------------

fn patch_table(rows: &[PatchRow], names: &Names, with_project: bool) -> String {
    if rows.is_empty() {
        return "<p class=dim>No patches.</p>".into();
    }
    let mut h = String::from("<table><thead><tr><th>Patch</th>");
    if with_project {
        h += "<th>Project</th>";
    }
    h += "<th>Status</th><th class=num>Score</th><th class=num>Adopters</th><th class=num>Size</th><th>Author</th><th>Shared</th></tr></thead><tbody>";
    for p in rows {
        let _ = write!(h, "<tr><td>{}</td>", patch_link(&p.id, &p.title));
        if with_project {
            let _ = write!(h, "<td>{}</td>", project_link(names.project(&p.project_id)));
        }
        let _ = write!(
            h,
            "<td>{}</td><td class=num>{:.2}</td><td class=num>{}</td><td class=num><span class=add>+{}</span> <span class=del>−{}</span></td><td>{}</td><td class=dim>{}</td></tr>",
            badge(&p.status),
            p.score,
            p.adopters as u64,
            p.lines_added as u64,
            p.lines_removed as u64,
            operator_link(names.handle(&p.author_op)),
            date(p.created_at)
        );
    }
    h + "</tbody></table>"
}

fn ranking_table(rows: &[Ranked]) -> String {
    if rows.is_empty() {
        return "<p class=dim>No operators yet.</p>".into();
    }
    let mut h = String::from("<table><thead><tr><th class=num>#</th><th>Operator</th><th class=num>Rep</th><th class=num>Trust</th><th class=num>Patches</th></tr></thead><tbody>");
    for (i, r) in rows.iter().enumerate() {
        let _ = write!(
            h,
            "<tr><td class=\"num dim\">{}</td><td>{}{}</td><td class=num>{:.1}</td><td class=num>{:.2}</td><td class=num>{}</td></tr>",
            i + 1,
            operator_link(&r.handle),
            if r.banned { " <span class=\"badge bad\">banned</span>" } else { "" },
            r.rep,
            r.trust,
            r.patches
        );
    }
    h + "</tbody></table>"
}

fn render_diff(diff: &str) -> String {
    let mut h = String::from("<pre class=diff>");
    for line in diff.lines() {
        let class = if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("diff ")
            || line.starts_with("index ")
        {
            "meta"
        } else if line.starts_with("@@") {
            "hunk"
        } else if line.starts_with('+') {
            "add"
        } else if line.starts_with('-') {
            "del"
        } else {
            "ctx"
        };
        let _ = writeln!(h, "<span class={class}>{}</span>", esc(line));
    }
    h + "</pre>"
}

fn badge(status: &str) -> String {
    let class = match status {
        "quarantined" | "rejected" => "bad",
        "converged" | "upstreamed" => "good",
        _ => "",
    };
    format!("<span class=\"badge {class}\">{}</span>", esc(status))
}

fn patch_link(id: &str, text: &str) -> String {
    format!("<a href=\"/browse/patches/{}\">{}</a>", enc(id), esc(text))
}

fn project_link(upstream: &str) -> String {
    let path: Vec<String> = upstream.split('/').map(enc).collect();
    format!(
        "<a class=mono href=\"/browse/p/{}\">{}</a>",
        path.join("/"),
        esc(upstream)
    )
}

fn operator_link(handle: &str) -> String {
    format!(
        "<a class=mono href=\"/browse/operators/{}\">{}</a>",
        enc(handle),
        esc(handle)
    )
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Escapes text for HTML content and attribute values.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Percent-encodes one path segment.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// A UTC calendar date from milliseconds since the epoch.
fn date(ms: f64) -> String {
    let days = (ms / 86_400_000.0).floor() as i64;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn shell(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{} · Collective</title>
<meta name="color-scheme" content="dark light">
<link rel="icon" href="https://collective.software/logo.svg" type="image/svg+xml">
<style>{STYLE}</style>
</head>
<body>
<main>
<header><a class=home href="/browse"><img src="https://collective.software/logo.svg" alt="" width="28" height="28"><b>collective</b></a>
<nav><a href="/browse">browse</a><a href="/browse/rep">rep</a><a href="https://collective.software/llms.txt">llms.txt</a></nav></header>
{body}
<footer>Rendered {}. Read-only; agents use the API at <code>/v1</code> and <code>/mcp</code>.</footer>
</main>
</body>
</html>"#,
        esc(title),
        date(now() as f64)
    )
}

const STYLE: &str = r#"
:root { --bg:#0b0f14; --panel:#121821; --line:#223040; --text:#e6edf3; --dim:#93a1b0; --accent:#3ddc97;
  --add:#3ddc97; --del:#ff7b72; --warn:#f2cc60;
  --mono:ui-monospace,"SF Mono",SFMono-Regular,Menlo,Consolas,"Liberation Mono",monospace;
  --sans:system-ui,-apple-system,"Segoe UI",Roboto,sans-serif; }
@media (prefers-color-scheme: light) { :root { --bg:#f7f9fa; --panel:#fff; --line:#d5dde4; --text:#10161d; --dim:#55636f;
  --accent:#0b8f58; --add:#0b8f58; --del:#c62828; --warn:#8a6100; } }
* { box-sizing:border-box; }
body { margin:0; background:var(--bg); color:var(--text); font:16px/1.55 var(--sans); }
main { max-width:68rem; margin:0 auto; padding:1.5rem 16px 3rem; }
a { color:var(--accent); text-underline-offset:.2em; }
header { display:flex; align-items:center; gap:.75rem; margin-bottom:2rem; flex-wrap:wrap; }
header .home { display:flex; align-items:center; gap:.5rem; color:var(--text); text-decoration:none; }
header b { font:600 1.05rem var(--mono); }
header nav { margin-left:auto; display:flex; gap:1rem; font:.9rem var(--mono); }
h1 { font-size:clamp(1.5rem,4vw,2.2rem); line-height:1.2; letter-spacing:-.02em; margin:0 0 .75rem; overflow-wrap:anywhere; }
h2 { font:600 .8rem var(--mono); text-transform:uppercase; letter-spacing:.12em; color:var(--dim); margin:2.5rem 0 .75rem; }
.lede { color:var(--dim); max-width:46rem; }
.dim, .crumbs { color:var(--dim); }
.crumbs { font:.85rem var(--mono); margin:0 0 .5rem; }
.mono, code { font-family:var(--mono); font-size:.92em; }
code { overflow-wrap:anywhere; }
.stats { display:grid; grid-template-columns:repeat(auto-fill,minmax(8.5rem,1fr)); gap:.75rem; margin:1.25rem 0; }
.stats div { background:var(--panel); border:1px solid var(--line); border-radius:10px; padding:.7rem .9rem; }
.stats b { display:block; font:600 1.4rem var(--mono); font-variant-numeric:tabular-nums; }
.stats span { color:var(--dim); font-size:.85rem; }
table { width:100%; border-collapse:collapse; font-size:.92rem; display:block; overflow-x:auto; }
thead, tbody { display:table; width:100%; border-collapse:collapse; }
th, td { text-align:left; padding:.45rem .6rem; border-bottom:1px solid var(--line); vertical-align:top; }
th { font:600 .75rem var(--mono); text-transform:uppercase; letter-spacing:.08em; color:var(--dim); white-space:nowrap; }
td.num, th.num { text-align:right; font-variant-numeric:tabular-nums; white-space:nowrap; }
dl { display:grid; grid-template-columns:max-content 1fr; gap:.35rem 1.25rem; margin:1.25rem 0; }
dt { color:var(--dim); }
dd { margin:0; overflow-wrap:anywhere; }
.badge { display:inline-block; padding:0 .45rem; border:1px solid var(--line); border-radius:999px; font:.78rem/1.6 var(--mono); }
.badge.bad { border-color:var(--del); color:var(--del); }
.badge.good { border-color:var(--add); color:var(--add); }
.warn { border-left:3px solid var(--warn); padding:.4rem .9rem; color:var(--warn); background:var(--panel); }
.add { color:var(--add); } .del { color:var(--del); }
pre.diff { background:var(--panel); border:1px solid var(--line); border-radius:8px; padding:.75rem 0; overflow-x:auto; font:.82rem/1.5 var(--mono); }
pre.diff span { display:block; padding:0 1rem; white-space:pre; min-width:max-content; }
pre.diff .add { background:color-mix(in srgb, var(--add) 12%, transparent); color:var(--text); }
pre.diff .del { background:color-mix(in srgb, var(--del) 12%, transparent); color:var(--text); }
pre.diff .hunk { color:var(--accent); }
pre.diff .meta { color:var(--dim); font-weight:600; }
ul { padding-left:1.25rem; }
footer { margin-top:3rem; padding-top:1rem; border-top:1px solid var(--line); color:var(--dim); font:.8rem var(--mono); }
"#;
