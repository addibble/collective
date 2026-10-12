//! One Durable Object per project: the single writer for convergence.
//! Adoptions mark a base commit dirty, and an alarm rebuilds the trunk for
//! every dirty base once activity settles, so a burst of votes costs one
//! rebuild.

use std::time::Duration;

use worker::*;

use crate::store::Store;

pub const CONVERGE_DEBOUNCE: Duration = Duration::from_secs(30);

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
        store
            .converge(project, base)
            .await
            .map_err(|e| Error::RustError(e.message))
    }
}

impl DurableObject for ProjectCoordinator {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let (Some(project), Some(base)) = (query(&req, "project"), query(&req, "base")) else {
            return Response::error("project and base required", 400);
        };
        let storage = self.state.storage();
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
