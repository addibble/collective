//! The search contract. Every operation is scoped to one project (the
//! partition key) and is either a batch of exact key lookups or a top-k
//! query, so each maps onto a sharded key-value store plus a namespaced
//! vector or full-text engine. See docs/SEARCH.md.
//!
//! The index is derived data: the system of record (patches, intents,
//! adoptions) can rebuild it at any time, which keeps backends swappable.
//! `D1Search` is the development backend.

use std::collections::HashMap;

use collective_core::diff::HunkFingerprint;
use collective_core::sketch::PatchSketch;
use collective_core::text::{similarity, terms};
use serde::Deserialize;

use crate::args;
use crate::db::{placeholders, Db, MAX_PARAMS};
use crate::error::ApiResult;

pub struct IntentHit {
    pub id: String,
    /// Relevance in [0, 1]: lexical, semantic or hybrid, depending on backend.
    pub relevance: f64,
}

pub struct HunkHit {
    pub patch_id: String,
    /// Fingerprints in common with the query.
    pub shared: u32,
    /// Fingerprints in the hit patch.
    pub total: u32,
}

pub struct SketchHit {
    pub patch_id: String,
    pub signature: String,
}

#[allow(async_fn_in_trait)] // single-threaded wasm: no Send bounds needed
pub trait SearchIndex {
    async fn index_intent(&self, project_id: &str, intent_id: &str, text: &str) -> ApiResult<()>;
    async fn index_patch(
        &self,
        project_id: &str,
        patch_id: &str,
        hunks: &[HunkFingerprint],
        sketch: Option<&PatchSketch>,
    ) -> ApiResult<()>;
    async fn search_intents(
        &self,
        project_id: &str,
        text: &str,
        k: usize,
    ) -> ApiResult<Vec<IntentHit>>;
    async fn patches_by_hunks(
        &self,
        project_id: &str,
        fingerprints: &[String],
        k: usize,
    ) -> ApiResult<Vec<HunkHit>>;
    /// Candidates sharing at least one LSH band; the caller verifies them by signature.
    async fn patches_by_bands(
        &self,
        project_id: &str,
        bands: &[String],
        k: usize,
    ) -> ApiResult<Vec<SketchHit>>;
}

pub struct D1Search<'a>(pub &'a Db);

impl SearchIndex for D1Search<'_> {
    async fn index_intent(&self, project_id: &str, intent_id: &str, text: &str) -> ApiResult<()> {
        let stmts = terms(text)
            .into_iter()
            .map(|t| self.0.stmt("INSERT OR IGNORE INTO intent_terms (term, intent_id, project_id) VALUES (?, ?, ?)", args![t, intent_id, project_id]))
            .collect::<ApiResult<Vec<_>>>()?;
        self.0.batch(stmts).await
    }

    async fn index_patch(
        &self,
        project_id: &str,
        patch_id: &str,
        hunks: &[HunkFingerprint],
        sketch: Option<&PatchSketch>,
    ) -> ApiResult<()> {
        let mut stmts = Vec::new();
        for f in hunks {
            stmts.push(self.0.stmt(
                "INSERT OR IGNORE INTO patch_hunks (project_id, path, fingerprint, patch_id) VALUES (?, ?, ?, ?)",
                args![project_id, &f.path, &f.fingerprint, patch_id],
            )?);
        }
        if let Some(sk) = sketch {
            stmts.push(self.0.stmt(
                "INSERT OR REPLACE INTO patch_sketches (patch_id, project_id, signature) VALUES (?, ?, ?)",
                args![patch_id, project_id, &sk.signature],
            )?);
            for b in &sk.bands {
                stmts.push(self.0.stmt("INSERT OR IGNORE INTO patch_bands (project_id, band, patch_id) VALUES (?, ?, ?)", args![project_id, b, patch_id])?);
            }
        }
        self.0.batch(stmts).await
    }

    async fn search_intents(
        &self,
        project_id: &str,
        text: &str,
        k: usize,
    ) -> ApiResult<Vec<IntentHit>> {
        let mut q = terms(text);
        q.truncate(MAX_PARAMS / 2);
        if q.is_empty() {
            return Ok(vec![]);
        }
        #[derive(Deserialize)]
        struct C {
            id: String,
            text: String,
        }
        let mut a = args![project_id];
        a.extend(q.iter().map(Into::into));
        let cands: Vec<C> = self
            .0
            .all(
                &format!(
                    "SELECT i.id, i.text FROM intents i
                     JOIN (SELECT intent_id, COUNT(*) AS hits FROM intent_terms
                           WHERE project_id = ? AND term IN ({}) GROUP BY intent_id ORDER BY hits DESC LIMIT 200) t
                     ON t.intent_id = i.id",
                    placeholders(q.len())
                ),
                a,
            )
            .await?;

        #[derive(Deserialize)]
        struct N {
            n: f64,
        }
        let total = self
            .0
            .first::<N>(
                "SELECT COUNT(*) AS n FROM intents WHERE project_id = ?",
                args![project_id],
            )
            .await?
            .map_or(0.0, |r| r.n);
        let mut vocab: Vec<String> = q.clone();
        for c in &cands {
            vocab.extend(terms(&c.text));
        }
        vocab.sort();
        vocab.dedup();
        let mut df: HashMap<String, f64> = HashMap::new();
        for chunk in vocab.chunks(MAX_PARAMS) {
            #[derive(Deserialize)]
            struct D {
                term: String,
                df: f64,
            }
            let mut a = args![project_id];
            a.extend(chunk.iter().map(Into::into));
            let rows: Vec<D> = self
                .0
                .all(&format!("SELECT term, COUNT(*) AS df FROM intent_terms WHERE project_id = ? AND term IN ({}) GROUP BY term", placeholders(chunk.len())), a)
                .await?;
            df.extend(rows.into_iter().map(|r| (r.term, r.df)));
        }
        let idf = |t: &str| (1.0 + (total + 1.0) / (df.get(t).copied().unwrap_or(0.0) + 1.0)).ln();
        let mut hits: Vec<IntentHit> = cands
            .into_iter()
            .map(|c| IntentHit {
                relevance: similarity(&q, &terms(&c.text), idf),
                id: c.id,
            })
            .collect();
        hits.sort_by(|a, b| b.relevance.total_cmp(&a.relevance));
        hits.truncate(k);
        Ok(hits)
    }

    async fn patches_by_hunks(
        &self,
        project_id: &str,
        fingerprints: &[String],
        k: usize,
    ) -> ApiResult<Vec<HunkHit>> {
        let fps = &fingerprints[..fingerprints.len().min(MAX_PARAMS)];
        if fps.is_empty() {
            return Ok(vec![]);
        }
        #[derive(Deserialize)]
        struct H {
            patch_id: String,
            shared: f64,
            total: f64,
        }
        let mut a = args![project_id];
        a.extend(fps.iter().map(Into::into));
        a.push(k.into());
        let rows: Vec<H> = self
            .0
            .all(
                &format!(
                    "SELECT h.patch_id, h.shared, (SELECT COUNT(*) FROM patch_hunks x WHERE x.patch_id = h.patch_id) AS total
                     FROM (SELECT patch_id, COUNT(*) AS shared FROM patch_hunks WHERE project_id = ? AND fingerprint IN ({})
                           GROUP BY patch_id ORDER BY shared DESC LIMIT ?) h",
                    placeholders(fps.len())
                ),
                a,
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| HunkHit {
                patch_id: r.patch_id,
                shared: r.shared as u32,
                total: r.total as u32,
            })
            .collect())
    }

    async fn patches_by_bands(
        &self,
        project_id: &str,
        bands: &[String],
        k: usize,
    ) -> ApiResult<Vec<SketchHit>> {
        if bands.is_empty() {
            return Ok(vec![]);
        }
        #[derive(Deserialize)]
        struct S {
            patch_id: String,
            signature: String,
        }
        let mut a = args![project_id];
        a.extend(bands.iter().map(Into::into));
        a.push(k.into());
        let rows: Vec<S> = self
            .0
            .all(
                &format!(
                    "SELECT s.patch_id, s.signature FROM patch_sketches s
                     JOIN (SELECT patch_id, COUNT(*) AS hits FROM patch_bands WHERE project_id = ? AND band IN ({})
                           GROUP BY patch_id ORDER BY hits DESC LIMIT ?) b ON b.patch_id = s.patch_id",
                    placeholders(bands.len())
                ),
                a,
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| SketchHit {
                patch_id: r.patch_id,
                signature: r.signature,
            })
            .collect())
    }
}
