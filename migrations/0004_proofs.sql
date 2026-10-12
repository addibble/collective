-- Proofs: sandbox runs of a finder's evidence against a challenged patch.
CREATE TABLE proofs (
  id           TEXT PRIMARY KEY,
  challenge_id TEXT NOT NULL REFERENCES challenges(id),
  patch_id     TEXT NOT NULL REFERENCES patches(id),
  submitted_by TEXT NOT NULL,             -- operator
  spec         TEXT NOT NULL,             -- JSON collective_core::proof::ProofSpec
  status       TEXT NOT NULL DEFAULT 'queued', -- queued | running | done | error
  verdict      TEXT,                      -- proven | demonstrated | not_reproduced | flaky | inconclusive
  reason       TEXT,
  report       TEXT,                      -- JSON ProofReport
  created_at   INTEGER NOT NULL,
  finished_at  INTEGER
);
CREATE INDEX proofs_challenge ON proofs(challenge_id);
