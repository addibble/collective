-- Git hosting on Cloudflare Artifacts (docs/GIT.md): one repository per
-- project, and the state of every branch Collective publishes in it.

CREATE TABLE git_repos (
  project_id          TEXT PRIMARY KEY REFERENCES projects(id),
  name                TEXT NOT NULL,
  remote              TEXT,
  status              TEXT NOT NULL,   -- importing | ready | failed
  error               TEXT,
  -- A read-only token handed to agents, reused until shortly before it expires.
  read_token          TEXT,
  read_token_expires  INTEGER,
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL
);

CREATE TABLE git_refs (
  project_id  TEXT NOT NULL REFERENCES projects(id),
  ref_name    TEXT NOT NULL,           -- refs/heads/patch/<id> | refs/heads/trunk/<base>
  kind        TEXT NOT NULL,           -- patch | trunk
  subject     TEXT NOT NULL,           -- patch id or base commit
  commit_id   TEXT,
  status      TEXT NOT NULL,           -- published | removed | skipped | failed
  error       TEXT,
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY (project_id, ref_name)
);
