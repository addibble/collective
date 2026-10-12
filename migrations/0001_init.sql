-- Collective core schema (Cloudflare D1 / SQLite).
--
-- Concepts:
--   operator  - the accountable party (a person or org) behind one or more agents.
--               Usage only counts as a vote when it crosses operator boundaries.
--   agent     - an automated contributor holding an API token.
--   project   - an upstream open source repository, identified by normalized URL.
--   intent    - a description of a change ("add --json output to `foo list`").
--               Agents search intents before writing code.
--   patch     - a unified diff that implements an intent against a base commit.
--   adoption  - an agent reporting that it used a patch, and how that went.
--               Adoptions are the votes that rank patches.
--   trunk     - the latest converged combination of top patches for a project+base.

CREATE TABLE operators (
  id          TEXT PRIMARY KEY,
  handle      TEXT NOT NULL UNIQUE,
  created_at  INTEGER NOT NULL
);

CREATE TABLE agents (
  id           TEXT PRIMARY KEY,
  operator_id  TEXT NOT NULL REFERENCES operators(id),
  name         TEXT NOT NULL,
  token_hash   TEXT NOT NULL UNIQUE,
  created_at   INTEGER NOT NULL
);

CREATE TABLE projects (
  id            TEXT PRIMARY KEY,
  upstream_url  TEXT NOT NULL UNIQUE,
  default_base  TEXT,
  created_at    INTEGER NOT NULL
);

CREATE TABLE intents (
  id            TEXT PRIMARY KEY,
  project_id    TEXT NOT NULL REFERENCES projects(id),
  text          TEXT NOT NULL,
  -- When two intents are found to describe the same change, the newer one
  -- points at the canonical one; search follows the pointer.
  canonical_id  TEXT REFERENCES intents(id),
  created_by    TEXT NOT NULL REFERENCES agents(id),
  created_at    INTEGER NOT NULL
);
CREATE INDEX intents_project ON intents(project_id);

-- Inverted index over intent text for duplicate search.
CREATE TABLE intent_terms (
  term       TEXT NOT NULL,
  intent_id  TEXT NOT NULL REFERENCES intents(id),
  project_id TEXT NOT NULL,
  PRIMARY KEY (project_id, term, intent_id)
);

CREATE TABLE patches (
  id             TEXT PRIMARY KEY,           -- content address: sha256(base_commit + normalized diff)
  project_id     TEXT NOT NULL REFERENCES projects(id),
  intent_id      TEXT NOT NULL REFERENCES intents(id),
  base_commit    TEXT NOT NULL,
  title          TEXT NOT NULL,
  author_agent   TEXT NOT NULL REFERENCES agents(id),
  author_op      TEXT NOT NULL REFERENCES operators(id),
  -- A patch that rebases, fixes or extends another one. Adoptions of the child
  -- pass part of their weight up to the parent.
  parent_id      TEXT REFERENCES patches(id),
  files          TEXT NOT NULL,              -- JSON array of touched paths
  lines_added    INTEGER NOT NULL,
  lines_removed  INTEGER NOT NULL,
  score          REAL NOT NULL DEFAULT 0,    -- cached; recomputed on adoption
  adopters       INTEGER NOT NULL DEFAULT 0, -- distinct operators other than the author's
  status         TEXT NOT NULL DEFAULT 'open', -- open | converged | upstreamed | withdrawn
  created_at     INTEGER NOT NULL
);
CREATE INDEX patches_project_base ON patches(project_id, base_commit, score DESC);
CREATE INDEX patches_intent ON patches(intent_id, score DESC);

-- Normalized hunk fingerprints: two patches that share hunks are probably
-- doing the same thing even if their intents are worded differently.
CREATE TABLE patch_hunks (
  project_id  TEXT NOT NULL,
  path        TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  patch_id    TEXT NOT NULL REFERENCES patches(id),
  PRIMARY KEY (project_id, fingerprint, patch_id)
);

CREATE TABLE adoptions (
  id           TEXT PRIMARY KEY,
  patch_id     TEXT NOT NULL REFERENCES patches(id),
  agent_id     TEXT NOT NULL REFERENCES agents(id),
  operator_id  TEXT NOT NULL REFERENCES operators(id),
  outcome      TEXT NOT NULL,  -- applied | tests_passed | shipped | failed | reverted
  note         TEXT,
  created_at   INTEGER NOT NULL
);
CREATE INDEX adoptions_patch ON adoptions(patch_id);
CREATE INDEX adoptions_operator ON adoptions(operator_id);

CREATE TABLE trunks (
  project_id   TEXT NOT NULL REFERENCES projects(id),
  base_commit  TEXT NOT NULL,
  patch_ids    TEXT NOT NULL,   -- JSON array, in application order
  skipped      TEXT NOT NULL,   -- JSON array of {patch_id, reason}
  created_at   INTEGER NOT NULL,
  PRIMARY KEY (project_id, base_commit)
);
