# Search at scale

Target: millions of upstream repositories and up to trillions of branches, with every agent running a search before it writes code. D1 cannot hold that; nor can any single database. This document describes how the index is shaped so that it *can* be held, and which engines serve each part.

## 1. Never index branches

A trillion branches is not a trillion things to search. Most branches are copies of each other: forks that mirror upstream, agents' scratch branches that hold the same change, the same change rebased onto ten different commits. The searchable unit is the **change**, and it is deduplicated in layers before anything reaches an index:

| Layer | Collapses | Key |
|---|---|---|
| Fork network | every fork of a repository into one partition | root repository of the fork network → `project_id` |
| Ref → commit | branches pointing at the same commit | commit SHA |
| Commit → change | branch tip vs. its merge-base with upstream mainline; identical diffs on the same base | `patch_id = sha256(base + normalized diff)` (content address) |
| Change across bases | the same edit rebased or cherry-picked | hunk fingerprints (and `git patch-id --stable`, which git already defines for this) |
| Near-identical edits | small variations of one edit | MinHash/LSH bands |
| Descriptions | reworded descriptions of the same goal | canonical intent |

A new branch whose change already exists costs one content-addressed lookup and a reference-count increment. **Index size grows with unique hunks, not with branches.** GitHub's code search reported the same effect: deduplicating blobs shrank roughly 115 TB of content to about 28 TB ([GitHub engineering blog, 2023](https://github.blog/2023-02-06-the-technology-behind-githubs-new-code-search/)).

## 2. Every query is partitioned and lookup-shaped

The `SearchIndex` interface (`src/search/types.ts`) allows only operations that scale horizontally:

| Query | What it answers | Shape | Partition |
|---|---|---|---|
| `patchesByHunks` | "someone made exactly this edit" | batch of exact key lookups | `(project, fingerprint)` |
| `patchesByBands` | "someone made nearly this edit" | 8 exact key lookups, then verify signatures | `(project, band)` |
| `searchIntents` | "someone wanted the same thing" | top-k ANN + BM25 within one namespace | `project` |

There are no scans, no cross-partition joins and no global sorts. An agent almost always knows which project it is changing, so the project (fork-network root) is the natural partition. Project sizes follow a power law: millions of tiny partitions and a few huge ones (kernels, browsers, compilers). The engines below are chosen for that distribution: cheap cold namespaces and sub-sharding for the giants.

Near-duplicate detection (`src/core/sketch.ts`) uses 32 MinHash permutations in 8 bands of 4. The chance that two patches become candidates is about `1 − (1 − s⁴)⁸`, where `s` is their similarity: 89% at s = 0.7, 40% at s = 0.5, under 7% at s = 0.3. Candidates are then checked against their full signatures. Each patch writes 8 keys and each query reads 8, whatever the corpus size.

## 3. Sizing

Assume an upper scenario: 10¹² branches deduplicate to 10¹⁰ unique patches, averaging 5 hunks each, with 10⁹ canonical intents.

| Index | Entries | Approx. size |
|---|---|---|
| Hunk postings | 5×10¹⁰ | ~2.5 TB |
| LSH band postings | 8×10¹⁰ | ~3 TB |
| MinHash signatures | 10¹⁰ × 128 B | ~1.3 TB |
| Intent vectors (1024-d) | 10⁹ | 4 TB float32 · 1 TB int8 · 128 GB binary-quantized |
| Diff bodies (R2) | 10¹⁰ | tens of TB, compressed |

All of this is well within reach of commodity sharded storage. Indexing per branch would multiply these figures by roughly 100.

**Hot keys.** Some hunks recur millions of times: version bumps, lockfile churn, generated code. These are excluded at ingestion (generated and lock files, and hunks below a minimum of changed content). Posting lists are capped at the top-N by score plus a total count.

## 4. Engines

The interface keeps these choices reversible. The D1 backend (`src/search/d1.ts`) is for development and small deployments only.

**Exact and LSH postings (key → small posting list).** These are point reads and writes, write-heavy, at 10¹¹ keys.
- *Cloudflare-native:* Durable Objects with SQLite storage, sharded by `hash(project, key)`. The per-project `ProjectCoordinator` already exists, and it can own the postings for the long tail of small projects. Large projects get dedicated shard objects. There is no separate service to operate.
- *If write throughput outgrows that:* a sharded log-structured database such as ScyllaDB, FoundationDB, Bigtable or DynamoDB. The access pattern is a textbook fit for any of them.

**Intent search (hybrid semantic + lexical, per project).** Run BM25 and vector search, then fuse the results with reciprocal rank fusion.
- *Recommended to evaluate first:* **turbopuffer**. It is object-storage-backed with one namespace per project, so millions of mostly idle namespaces stay cheap, and it supports both vector and full-text queries.
- *Alternatives:* **Vespa** (the most capable hybrid ranking; heaviest to operate), **LanceDB on R2** (embedded and Cloudflare-adjacent; less mature at this scale), **OpenSearch k-NN** (familiar, but costly at billions of vectors), and **Cloudflare Vectorize** with Workers AI embeddings (simplest on-platform, but check its per-index vector and namespace limits against the target).
- Embeddings come from a code-aware model applied to intent text, commit messages and PR titles. Binary quantization plus re-scoring keeps the vector tier small.

**Global, cross-project discovery** ("has anyone added OpenTelemetry tracing to a Flask app?"). This is a separate, smaller tier indexing only canonical intents whose patches pass a usage threshold (roughly the top 1%), across all projects. The long tail stays in per-project namespaces and is reached only through a project-scoped query.

**Code search over trunks and top patches.** This is regex and identifier search, done with a trigram/n-gram index (Zoekt, or a Blackbird-style design), over ranked content only, never over every branch.

## 5. Ranking

```
rank = relevance(text, hunk overlap, sketch similarity)
     × usage(score, adopters)          -- core/score.ts
     × proximity(patch base, agent HEAD)
     × freshness
```

**Base proximity** answers whether this patch was made against a commit close to the one the agent has checked out. Each mainline commit is stored with its position in the project's first-parent history, which makes distance `|pos(a) − pos(b)|` an O(1) lookup. A patch on a side branch maps to its merge-base with mainline.

**Learning to rank.** Every adoption that follows a search is a labelled training example: which result was shown, which one was used, and how it went. Hand-set weights can be replaced by a model trained on those labels once there is volume. Few search systems get labels this clean.

## 6. Ingestion

```
push / submit / webhook
   → Queue
   → indexer Worker:
        resolve fork-network root      (project)
        merge-base with mainline       (base)
        normalized diff → patch_id     ── exists? → ref count++ ; done
        fingerprints, sketch           → postings shards
        intent text → embedding        → project namespace (+ global tier if ranked)
        diff                           → R2
   adoption events → rescore → update rank fields in place (no re-embedding)
```

Ingestion is idempotent because everything is content-addressed. The system of record is an append-only event log on R2 (Iceberg tables via R2 Data Catalog are a natural fit). Every index is derived from that log and can be rebuilt by replaying it, which is also how backends get migrated.

The system of record scales the same way. Per-project metadata (patches, intents, adoptions) moves out of the single D1 database and into each project's Durable Object, with diff bodies in R2.

## 7. Staging

| Stage | Scale | Change |
|---|---|---|
| 0 (now) | prototype | D1 backend behind `SearchIndex`; exact + LSH duplicate detection; lexical intents |
| 1 | 10³–10⁵ projects | Queue-based ingestion; per-project DO SQLite for metadata and postings; embeddings plus a namespaced vector/BM25 engine; fork-network resolution |
| 2 | 10⁶+ projects, 10¹⁰ patches | dedicated postings shards; global tier; trigram code search over trunks; learned ranking from adoptions |

## 8. How we will know it works

Before choosing engines, we need a benchmark of real duplicate changes. Candidate sources:
- pull requests closed as duplicates;
- cherry-picks between forks (equal `git patch-id`s);
- the same fix landing in several forks of one fork network (GH Archive, Software Heritage).

Measure recall@k and latency at each stage. The LSH bands and thresholds in `core/sketch.ts` should be tuned against that set rather than by intuition.

## Open questions

- **Agent HEAD privacy.** Searching by diff reveals what an agent is working on. Hashing fingerprints client-side and querying by fingerprint alone would let agents search without uploading code.
- **Poisoning the index.** Flooding near-duplicates to crowd out a good patch. Ranking by usage rather than count mitigates this, and per-operator ingestion quotas are still needed.
- **Projects without a mainline** (no upstream, or an abandoned one). These need a different notion of base.
