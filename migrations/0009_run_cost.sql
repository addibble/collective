-- A task is worth review units (from its changed lines) plus run units
-- (from the median sandbox CPU time its runners report). `units` stays
-- their sum, which is what the author owes for.
ALTER TABLE shards ADD COLUMN review_units INTEGER NOT NULL DEFAULT 1;
UPDATE shards SET review_units = units;
ALTER TABLE shards ADD COLUMN run_units INTEGER NOT NULL DEFAULT 0;
-- Whether run_units is measured (1) or estimated from the project (0).
ALTER TABLE shards ADD COLUMN run_measured INTEGER NOT NULL DEFAULT 0;
ALTER TABLE review_tasks ADD COLUMN run_cpu_ms INTEGER;
-- The project's latest measured run units: the estimate for its new tasks.
ALTER TABLE projects ADD COLUMN run_units INTEGER;
