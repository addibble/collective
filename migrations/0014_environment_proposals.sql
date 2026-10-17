-- Environment proposals (ENVIRONMENTS.md, Proposals): a `proposed` version
-- is a job for runners, keyed by (project_id, version), that runs the
-- project's suite in the proposed environment on one base commit.
-- The commit the suite is proven on, fixed when the proposal is made so
-- every runner runs the same tree.
ALTER TABLE project_environments ADD COLUMN base_commit TEXT;
-- The RunOutcome a quorum of runners settled on (`inconclusive` when the
-- proposal lapsed without one); late runs are graded against it.
ALTER TABLE project_environments ADD COLUMN outcome TEXT;
CREATE INDEX project_environments_state ON project_environments(state, created_at);
-- The project's suite spec as it was when the proposal was made (kind
-- `suite`, without an environment); the job's spec is this with the
-- proposed environment and one repeat.
ALTER TABLE project_environments ADD COLUMN run_spec TEXT;
