-- Environment quarantine (ENVIRONMENTS.md, Quarantine). An admin sets a
-- project's current version to `quarantined`: the project is review-only
-- (tasks without a run phase; proofs and canary verifications refused) and
-- no new run is offered in that environment. Operators with standing
-- contest it with bonds, and a quorum of stakes settles it.

-- When and why the version was quarantined (kept after a lift, as history).
ALTER TABLE project_environments ADD COLUMN quarantined_at INTEGER;
ALTER TABLE project_environments ADD COLUMN quarantine_reason TEXT;
-- How the contest settled, claimed once: 'lift' (a quorum returned it to
-- `adopted`), 'keep' (a quorum kept it quarantined), 'timeout' (no quorum
-- in time; every bond returned, the quarantine stands) or 'admin' (an admin
-- lifted the quarantine before anything settled; every bond returned).
-- NULL while the contest is open.
ALTER TABLE project_environments ADD COLUMN contest_settled TEXT;
-- When the quarantine was lifted, and by whom: 'admin' or 'contest'.
ALTER TABLE project_environments ADD COLUMN quarantine_lifted_at INTEGER;
ALTER TABLE project_environments ADD COLUMN quarantine_lifted_by TEXT;

-- One stake per operator per quarantined version. The bond is debited
-- (`contest_bond` in the ledger) in the same batch as the insert; `trust` is
-- the operator's trust when it staked, which the quorum counts. A version
-- quarantined again starts a new contest: its settled stakes are removed
-- (their ledger entries remain, under the old quarantine's reference).
CREATE TABLE environment_contests (
  project_id  TEXT NOT NULL,
  version     INTEGER NOT NULL,
  operator_id TEXT NOT NULL,
  side        TEXT NOT NULL,             -- lift | keep
  bond        REAL NOT NULL,
  trust       REAL NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (project_id, version, operator_id)
);
-- The timeout sweep reads quarantined versions by age.
CREATE INDEX project_environments_quarantine ON project_environments(state, quarantined_at);

-- A review task created while its project's environment is quarantined has
-- no run phase, whether or not the project has a test suite.
ALTER TABLE shards ADD COLUMN review_only INTEGER NOT NULL DEFAULT 0;
