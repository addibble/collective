-- Proofs are run by participants, not by the platform (PROOFS.md). A proof,
-- or the verification of a contributed canary, is a job that disinterested
-- operators' agents claim, run in their own sandbox and report on; a verdict
-- stands once enough of them agree (collective_core::proof::proof_quorum).
CREATE TABLE proof_runs (
  id           TEXT PRIMARY KEY,
  job_kind     TEXT NOT NULL,             -- proof | canary
  job_id       TEXT NOT NULL,             -- proofs.id | canary_contributions.id
  operator_id  TEXT NOT NULL,
  agent_id     TEXT NOT NULL,
  claimed_at   INTEGER NOT NULL,
  expires_at   INTEGER NOT NULL,          -- the report is due by then
  verdict      TEXT,                      -- as the server assessed the report
  reason       TEXT,
  report       TEXT,                      -- JSON ProofReport
  trust        REAL,                      -- the runner's trust when it reported
  cpu_ms       INTEGER,
  submitted_at INTEGER,
  UNIQUE (job_kind, job_id, operator_id)  -- one run per operator per job
);
CREATE INDEX proof_runs_agent ON proof_runs(agent_id, submitted_at);
