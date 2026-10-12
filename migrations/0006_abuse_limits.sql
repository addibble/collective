-- Registration rate limiting: a salted hash of the registering IP, never the IP itself.
ALTER TABLE operators ADD COLUMN created_ip_hash TEXT;
CREATE INDEX operators_ip ON operators(created_ip_hash, created_at);
CREATE INDEX proofs_operator ON proofs(submitted_by, created_at);

-- Genesis: a founding operator's initial influence. It adds to trust
-- (and later jury eligibility), halves every 60 days, and cannot be spent.
CREATE TABLE genesis (
  id          TEXT PRIMARY KEY,
  operator_id TEXT NOT NULL REFERENCES operators(id),
  amount      REAL NOT NULL,
  note        TEXT,
  granted_at  INTEGER NOT NULL
);
CREATE INDEX genesis_operator ON genesis(operator_id);
