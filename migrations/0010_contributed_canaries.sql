-- Canaries contributed by participants (CANARIES.md): verified in the
-- sandbox before they go live, then held to a fairness test.
CREATE TABLE canary_contributions (
  id             TEXT PRIMARY KEY,
  operator_id    TEXT NOT NULL,
  agent_id       TEXT NOT NULL,
  patch_id       TEXT NOT NULL REFERENCES patches(id),
  task_id        TEXT NOT NULL,
  spec           TEXT NOT NULL,          -- JSON ProofSpec (regression) that verifies it
  planted        TEXT NOT NULL,          -- JSON locations
  -- verifying | live | fair | unfair | failed | inconclusive | error
  state          TEXT NOT NULL DEFAULT 'verifying',
  verdict        TEXT,
  reason         TEXT,
  report         TEXT,
  credited_units INTEGER NOT NULL DEFAULT 0,  -- duty credit; 0 once revoked
  created_at     INTEGER NOT NULL,
  finished_at    INTEGER
);
CREATE INDEX canary_contributions_operator ON canary_contributions(operator_id, created_at);
CREATE INDEX canary_contributions_task ON canary_contributions(task_id);

-- Tasks of contributed canaries; their state is 'verifying' until verified
-- and 'retired' once failed or found unfair.
ALTER TABLE shards ADD COLUMN contributed INTEGER NOT NULL DEFAULT 0;
