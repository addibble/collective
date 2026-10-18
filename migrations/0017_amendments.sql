-- Amendments (REVIEWS.md, Amendments): a reviewer's fix on top of the
-- patch it is reviewing, as the whole patch with the fix applied. The
-- rest of the panel endorses or rejects it; an accepted one becomes a
-- derived patch (patches.parent_id) by the amender.

CREATE TABLE amendments (
  id              TEXT PRIMARY KEY,
  shard_id        TEXT NOT NULL,          -- the task it was attached to
  patch_id        TEXT NOT NULL,          -- the patch under review
  operator_id     TEXT NOT NULL,          -- the amender
  agent_id        TEXT NOT NULL,
  category        TEXT NOT NULL,          -- performance | correctness
  evidence        TEXT NOT NULL,          -- JSON (collective_core::amend::Evidence)
  amended_id      TEXT NOT NULL,          -- content address of the amended patch
  lines           INTEGER NOT NULL,       -- lines it changes about the patch
  state           TEXT NOT NULL,          -- pending | accepted | rejected | lapsed
  result_patch_id TEXT,                   -- the derived patch, once accepted
  created_at      INTEGER NOT NULL,
  resolved_at     INTEGER
);
CREATE INDEX amendments_shard ON amendments(shard_id, state);
CREATE INDEX amendments_result ON amendments(result_patch_id);
CREATE INDEX amendments_operator ON amendments(operator_id, shard_id);

-- One vote per panel member per amendment; a repeated vote replaces it
-- while the amendment is pending.
CREATE TABLE amendment_votes (
  amendment_id TEXT NOT NULL,
  operator_id  TEXT NOT NULL,
  verdict      TEXT NOT NULL,             -- endorse | reject
  note         TEXT,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY (amendment_id, operator_id)
);
