//! One Durable Object per project: the single writer for convergence and
//! for the project's Git repository. Adoptions mark a base commit dirty, and
//! an alarm rebuilds the trunk for every dirty base once activity settles,
//! so a burst of votes costs one rebuild. Git hosting jobs (crate::git) run
//! from the same alarm, one at a time.

use std::time::Duration;

use worker::*;

use crate::git::Job;
use crate::store::Store;

pub const CONVERGE_DEBOUNCE: Duration = Duration::from_secs(30);
/// How soon queued git jobs start, and how long a repository that is still
/// importing waits before the next try.
const GIT_DELAY: Duration = Duration::from_secs(1);
const GIT_RETRY: Duration = Duration::from_secs(30);
/// Git jobs per alarm run; the rest wait for the next run.
const GIT_JOBS_PER_RUN: usize = 20;

#[durable_object]
pub struct ProjectCoordinator {
    state: State,
    env: Env,
}

fn query(req: &Request, key: &str) -> Option<String> {
    req.url()
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

impl ProjectCoordinator {
    async fn converge(&self, project: &str, base: &str) -> Result<serde_json::Value> {
        let store = Store::new(&self.env).map_err(|e| Error::RustError(e.message))?;
        let trunk = store
            .converge(project, base)
            .await
            .map_err(|e| Error::RustError(e.message))?;
        if crate::git::enabled(&self.env) {
            self.queue(Job::Trunk(base.to_string())).await?;
        }
        Ok(trunk)
    }

    async fn queue(&self, job: Job) -> Result<()> {
        let storage = self.state.storage();
        let mut jobs: Vec<Job> = storage.get("git_jobs").await?.unwrap_or_default();
        if !jobs.contains(&job) {
            jobs.push(job);
            storage.put("git_jobs", &jobs).await?;
        }
        if storage.get_alarm().await?.is_none() {
            storage.set_alarm(GIT_DELAY).await?;
        }
        Ok(())
    }

    /// Runs queued git jobs, taking each off the queue before it starts so
    /// jobs queued meanwhile aren't lost. Returns whether any are left.
    async fn run_git(&self, project: &str) -> Result<Option<Duration>> {
        let storage = self.state.storage();
        let mut retry = Vec::new();
        for _ in 0..GIT_JOBS_PER_RUN {
            let mut jobs: Vec<Job> = storage.get("git_jobs").await?.unwrap_or_default();
            if jobs.is_empty() {
                break;
            }
            let job = jobs.remove(0);
            storage.put("git_jobs", &jobs).await?;
            if let Err(why) = crate::git::run(&self.env, project, &job).await {
                console_log!("git hosting: {job:?} for {project} waits: {why}");
                retry.push(job);
                // Everything else needs the repository too.
                break;
            }
        }
        let mut jobs: Vec<Job> = storage.get("git_jobs").await?.unwrap_or_default();
        let waiting = !retry.is_empty();
        for job in retry.into_iter().rev() {
            if !jobs.contains(&job) {
                jobs.insert(0, job);
            }
        }
        storage.put("git_jobs", &jobs).await?;
        Ok(match (jobs.is_empty(), waiting) {
            (true, _) => None,
            (false, true) => Some(GIT_RETRY),
            (false, false) => Some(GIT_DELAY),
        })
    }
}

impl DurableObject for ProjectCoordinator {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let storage = self.state.storage();
        if req.path() == "/git" {
            let Some(project) = query(&req, "project") else {
                return Response::error("project required", 400);
            };
            let job: Job = req.json().await?;
            storage.put("project", &project).await?;
            self.queue(job).await?;
            return Response::empty();
        }
        let (Some(project), Some(base)) = (query(&req, "project"), query(&req, "base")) else {
            return Response::error("project and base required", 400);
        };
        match req.path().as_str() {
            "/dirty" => {
                storage.put("project", &project).await?;
                let mut dirty: Vec<String> = storage.get("dirty").await?.unwrap_or_default();
                if !dirty.contains(&base) {
                    dirty.push(base);
                    storage.put("dirty", &dirty).await?;
                }
                if storage.get_alarm().await?.is_none() {
                    storage.set_alarm(CONVERGE_DEBOUNCE).await?;
                }
                Response::empty()
            }
            "/converge" => {
                let mut dirty: Vec<String> = storage.get("dirty").await?.unwrap_or_default();
                dirty.retain(|b| b != &base);
                storage.put("dirty", &dirty).await?;
                Response::from_json(&self.converge(&project, &base).await?)
            }
            _ => Response::error("not found", 404),
        }
    }

    async fn alarm(&self) -> Result<Response> {
        let storage = self.state.storage();
        let Some(project) = storage.get::<String>("project").await? else {
            return Response::empty();
        };
        let dirty: Vec<String> = storage.get("dirty").await?.unwrap_or_default();
        storage.put("dirty", Vec::<String>::new()).await?;
        for base in dirty {
            self.converge(&project, &base).await?;
        }
        if let Some(next) = self.run_git(&project).await? {
            // A dirty base may already have set an alarm; the earlier one wins.
            if storage.get_alarm().await?.is_none() {
                storage.set_alarm(next).await?;
            }
        }
        Response::empty()
    }
}

pub async fn mark_dirty(env: &Env, project: &str, base: &str) -> Result<()> {
    let stub = env
        .durable_object("PROJECT")?
        .id_from_name(project)?
        .get_stub()?;
    stub.fetch_with_str(&format!(
        "https://coordinator/dirty?project={project}&base={base}"
    ))
    .await?;
    Ok(())
}

pub async fn converge_now(env: &Env, project: &str, base: &str) -> Result<serde_json::Value> {
    let stub = env
        .durable_object("PROJECT")?
        .id_from_name(project)?
        .get_stub()?;
    stub.fetch_with_str(&format!(
        "https://coordinator/converge?project={project}&base={base}"
    ))
    .await?
    .json()
    .await
}

pub async fn enqueue_git(env: &Env, project: &str, job: &Job) -> Result<()> {
    let stub = env
        .durable_object("PROJECT")?
        .id_from_name(project)?
        .get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(serde_json::to_string(job)?.into()));
    stub.fetch_with_request(Request::new_with_init(
        &format!("https://coordinator/git?project={project}"),
        &init,
    )?)
    .await?;
    Ok(())
}
