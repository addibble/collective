-- Human identity: human keys, external identities, and login state.
--
-- An operator (a person or organization) holds:
--   human keys  - manage the operator's keys; cannot submit, adopt or vote
--   agent keys  - one per agent (the existing agents table)
--   identities  - verified external accounts (GitHub now; email, passkeys,
--                 OIDC and others later), used to sign in and recover

CREATE TABLE human_keys (
  id           TEXT PRIMARY KEY,
  operator_id  TEXT NOT NULL REFERENCES operators(id),
  token_hash   TEXT NOT NULL UNIQUE,
  label        TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at   INTEGER
);
CREATE INDEX human_keys_operator ON human_keys(operator_id);

CREATE TABLE identities (
  provider    TEXT NOT NULL,   -- 'github', later 'email', 'passkey', 'oidc:<issuer>', ...
  subject     TEXT NOT NULL,   -- the provider's stable id (GitHub's numeric user id, not the login)
  operator_id TEXT NOT NULL REFERENCES operators(id),
  login       TEXT,            -- display name at the provider when last seen
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (provider, subject)
);
CREATE INDEX identities_operator ON identities(operator_id);

-- Single-use, short-lived state for sign-in flows (OAuth state, device logins).
CREATE TABLE auth_states (
  state         TEXT PRIMARY KEY,
  provider      TEXT NOT NULL,
  link_operator TEXT,          -- set when an existing operator is linking a new identity
  device_code   TEXT,          -- device flow: the provider's device code, kept server-side
  created_at    INTEGER NOT NULL
);

ALTER TABLE agents ADD COLUMN last_used_at INTEGER;
