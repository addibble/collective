-- Review tasks cover whole patches (at most 2000 changed lines) and have
-- two phases: review, then a sandboxed run of the project's test suite.
-- The `shards` table now holds one task per patch.

-- Work units a task is worth: one per 500 changed lines, at least one.
ALTER TABLE shards ADD COLUMN units INTEGER NOT NULL DEFAULT 1;
UPDATE shards SET units = MAX(1, (changed_lines + 499) / 500);
-- Canaries: the known outcome of running the suite with the canary applied.
ALTER TABLE shards ADD COLUMN expected_run TEXT;
-- Real tasks: the outcome most runners agreed on, once graded ('none' if no majority).
ALTER TABLE shards ADD COLUMN run_majority TEXT;

-- review: reading; run: review submitted, sandbox run owed; done.
ALTER TABLE review_tasks ADD COLUMN phase TEXT NOT NULL DEFAULT 'done';
-- A task is credited when both its review and its run (if one is owed) count.
ALTER TABLE review_tasks ADD COLUMN review_ok INTEGER NOT NULL DEFAULT 1;
ALTER TABLE review_tasks ADD COLUMN run_ok INTEGER NOT NULL DEFAULT 1;
UPDATE review_tasks SET review_ok = credited WHERE graded = 1;
UPDATE review_tasks SET phase = 'review' WHERE submitted_at IS NULL;
ALTER TABLE review_tasks ADD COLUMN run_outcome TEXT;
ALTER TABLE review_tasks ADD COLUMN run_report TEXT;
ALTER TABLE review_tasks ADD COLUMN run_submitted_at INTEGER;

-- How to run a project's test suite (JSON collective_core::proof::ProofSpec,
-- kind "suite"). Projects without one get review-only tasks.
ALTER TABLE projects ADD COLUMN run_spec TEXT;
