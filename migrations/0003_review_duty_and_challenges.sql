-- Review duty, challenges, reputation ledger and bans.

ALTER TABLE operators ADD COLUMN banned_at INTEGER;
ALTER TABLE operators ADD COLUMN ban_reason TEXT;
ALTER TABLE agents ADD COLUMN revoked_at INTEGER;

-- Reputation ("rep") is the sum of an operator's ledger entries. The
-- platform bounty pool is the pseudo-operator 'platform:pool'.
CREATE TABLE ledger (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  operator_id TEXT NOT NULL,
  delta       REAL NOT NULL,
  reason      TEXT NOT NULL,
  ref         TEXT,
  created_at  INTEGER NOT NULL
);
CREATE INDEX ledger_operator ON ledger(operator_id);

-- A bounded unit of review work: consecutive hunks of one file of a patch.
-- Canary shards belong to hidden, inert, known-bad patches.
CREATE TABLE shards (
  id            TEXT PRIMARY KEY,
  patch_id      TEXT NOT NULL REFERENCES patches(id),
  idx           INTEGER NOT NULL,
  path          TEXT NOT NULL,
  hunk_start    INTEGER NOT NULL,
  hunk_end      INTEGER NOT NULL,
  changed_lines INTEGER NOT NULL,
  author_op     TEXT NOT NULL,
  wanted        INTEGER NOT NULL,       -- reviewers wanted; grows when a lone serious report widens review
  canary        INTEGER NOT NULL DEFAULT 0,
  planted       TEXT,                   -- JSON locations of the planted problem (canaries only)
  state         TEXT NOT NULL DEFAULT 'open', -- open | challenged | clean | graded
  created_at    INTEGER NOT NULL
);
CREATE INDEX shards_open ON shards(state, created_at);
CREATE INDEX shards_patch ON shards(patch_id);

CREATE TABLE review_tasks (
  shard_id     TEXT NOT NULL REFERENCES shards(id),
  operator_id  TEXT NOT NULL,
  agent_id     TEXT NOT NULL,
  leased_at    INTEGER NOT NULL,
  expires_at   INTEGER NOT NULL,
  submitted_at INTEGER,
  issues       TEXT,                    -- JSON; [] means "clean"
  credited     INTEGER NOT NULL DEFAULT 0, -- counts towards duty (revoked if graded as failed)
  graded       INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (shard_id, operator_id)
);
CREATE INDEX review_tasks_operator ON review_tasks(operator_id, credited);

CREATE TABLE challenges (
  id         TEXT PRIMARY KEY,
  patch_id   TEXT NOT NULL REFERENCES patches(id),
  shard_id   TEXT,                       -- set when opened by review quorum
  location   TEXT,                       -- JSON location of the finding, when known
  state      TEXT NOT NULL,              -- JSON of collective_core::challenge::Challenge
  phase      TEXT NOT NULL,
  settled    INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);
CREATE INDEX challenges_patch ON challenges(patch_id);
