-- Environments (ENVIRONMENTS.md): the system layer a proof or review run
-- executes in, a Nix package set or the stock prover image. A project's
-- environment is versioned, never edited; every job pins the version it
-- was created under, and the server fills the spec from that pin.
CREATE TABLE project_environments (
  id           TEXT NOT NULL,            -- env_… (content id of the canonical record)
  project_id   TEXT NOT NULL REFERENCES projects(id),
  version      INTEGER NOT NULL,         -- per project, from 1
  spec         TEXT NOT NULL,            -- JSON collective_core::environment::Environment, canonical
  state        TEXT NOT NULL,            -- proposed | adopted | refused | quarantined
  proposed_by  TEXT,                     -- operator, NULL when set by an admin
  created_at   INTEGER NOT NULL,
  decided_at   INTEGER,
  reason       TEXT,
  PRIMARY KEY (project_id, version)
);
CREATE INDEX project_environments_id ON project_environments(id);

-- The adopted version's id; NULL is the stock image.
ALTER TABLE projects ADD COLUMN environment_id TEXT;
-- Pinned at creation; NULL is the stock image.
ALTER TABLE proofs ADD COLUMN environment_id TEXT;
ALTER TABLE shards ADD COLUMN environment_id TEXT;
ALTER TABLE canary_contributions ADD COLUMN environment_id TEXT;
-- 'stock' or 'nix', from the pinned version, so task assignment can match
-- a reviewer's capabilities without parsing records. Set on every new task;
-- every task before this migration ran in the stock image.
ALTER TABLE shards ADD COLUMN environment_kind TEXT;
UPDATE shards SET environment_kind = 'stock';
