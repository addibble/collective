-- Near-duplicate detection: MinHash signatures and their LSH band keys.
-- In production these live in the search backend, not the system of record;
-- see docs/SEARCH.md.

CREATE TABLE patch_sketches (
  patch_id   TEXT PRIMARY KEY REFERENCES patches(id),
  project_id TEXT NOT NULL,
  signature  TEXT NOT NULL      -- 32 x 32-bit minhashes, hex
);

CREATE TABLE patch_bands (
  project_id TEXT NOT NULL,
  band       TEXT NOT NULL,      -- "<band index>:<hash>"
  patch_id   TEXT NOT NULL REFERENCES patches(id),
  PRIMARY KEY (project_id, band, patch_id)
);
