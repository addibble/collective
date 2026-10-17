# Plan: Nix environments for proofs and review runs

Implementation plan for the design in `docs/ENVIRONMENTS.md`. Each work package (WP) is sized for one agent in one session, names the files it touches and the commands that prove it done, and lists the packages it depends on. Packages in the same phase with no dependency between them can run in parallel.

Read first: `CLAUDE.md`, `docs/ENVIRONMENTS.md`, `docs/PROOFS.md` (The sandbox, Who runs a proof), `docs/REVIEWS.md` (Review, then run). Rules that apply to every package:

- Protocol rules go in `crates/core` with tests. The Worker loads, stores and calls the core. The prover is the only thing that runs Nix.
- `crates/core` stays pure: no I/O, no clocks, no reading the filesystem. Where the prover needs a pure decision about what it found on disk, the core takes the facts as arguments.
- Fail closed. Anything that can't be realised, verified or pinned makes the run `inconclusive`, never "stock environment instead".
- Migrations are append-only: new numbered file, never edit an applied one. D1 numbers deserialise as `f64`.
- Keep docs in step with behaviour in the same package that changes the behaviour: README for usage, `docs/*.md` for design and what is and is not verified, `site/llms.txt` for what agents are told.
- Run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo clippy -p collective-worker -p collective-runner --target wasm32-unknown-unknown -- -D warnings` before finishing. On a Mac where Homebrew's `rustc` shadows rustup's, prefix `PATH="$HOME/.cargo/bin:$PATH"` for wasm targets.
- Don't weaken a check to make a test pass. Don't commit canary selections or `.dev.vars`.

## Decisions already made (don't reopen)

1. Nix package sets are the only environment mechanism. No Dockerfiles, no attested images in this plan.
2. `Ecosystem` stays as it is: it names the package-manager fetch (`cargo fetch`, `npm ci`, …). `Environment` is orthogonal and provides the system layer. The stock prover image is the environment for projects that declare none.
3. The environment travels **inside `ProofSpec`** as a new field `environment`, defaulting to stock. The server fills it from the project record when it hands out a job or task, and overwrites whatever a submitter put there. Prover, runner and CLI therefore keep their request shapes.
4. Realisation is `nix build --max-jobs 0 --no-link --print-out-paths` of `github:NixOS/nixpkgs/<rev>?narHash=<hash>#<pkg>` per package, substituting from `cache.nixos.org` only. The prover composes `PATH`, `PKG_CONFIG_PATH`, `NIX_CFLAGS_COMPILE` and `NIX_LDFLAGS` itself. No `nix shell`, no `nix develop`.
5. Runner capability is a single flag, `nix`. There is no per-environment catalogue.
6. Proposing an environment needs standing (`MIN_PROPOSER_TRUST`, start at 0.5); the proposal is proven by a quorum of runners passing the project's suite in it. Admins can set an environment directly for bootstrap.

## Shared vocabulary

- **Environment record**: `{"nix": {"nixpkgs": <40 hex>, "nixpkgs_hash": "sha256-…" | null, "packages": [..]}}` or `{"stock": {}}`. Canonical form: packages sorted and de-duplicated. **Environment id**: `sha256` of the canonical JSON, hex, first 32 chars, prefixed `env_`. Identical proposals share an id.
- **Environment version**: a row in `project_environments`, per project, numbered from 1; `projects.environment_id` points at the adopted one (NULL = stock).
- **Capability**: the string `nix`, sent by runners and reviewers as a query parameter `capabilities=nix` on `POST /v1/proofs/next` and `POST /v1/reviews/next`.

## Phase 0: foundations (parallel)

### WP0a: core `Environment` type and composition rules

Files: new `crates/core/src/environment.rs`, `crates/core/src/lib.rs` (export), new `crates/core/tests/environment.rs`.

Add:

- `pub enum Environment { Stock, Nix(NixEnvironment) }` with serde (`#[serde(rename_all = "snake_case")]`, externally tagged as in the record above, `Default = Stock`).
- `pub struct NixEnvironment { pub nixpkgs: String, pub nixpkgs_hash: Option<String>, pub packages: Vec<String> }`.
- `Environment::validate(&self) -> Result<(), String>`: `nixpkgs` is exactly 40 lowercase hex; `nixpkgs_hash`, if present, matches `sha256-` + 44 base64 chars; 1 to 64 packages; each package matches `^[A-Za-z0-9_][A-Za-z0-9_.-]*$`, has no empty segment between dots, is at most 128 chars; Stock always valid.
- `Environment::canonical(self) -> Environment` (sort, dedup) and `Environment::id(&self) -> String` as defined above (use `crate::hash`).
- `NixEnvironment::flake_refs(&self) -> Vec<String>`: `github:NixOS/nixpkgs/<rev>?narHash=<hash>#<pkg>` (omit the query when the hash is absent).
- `NixEnvironment::realise_argv(&self) -> Vec<String>`: `["nix", "build", "--max-jobs", "0", "--no-link", "--print-out-paths", "--option", "allow-import-from-derivation", "false", <refs…>]`.
- `pub struct StorePath { pub path: String, pub has_bin: bool, pub has_lib: bool, pub has_include: bool, pub has_lib_pkgconfig: bool, pub has_share_pkgconfig: bool }` and `pub fn compose_env(paths: &[StorePath]) -> Vec<(String, String)>`: deterministic, in input order: `PATH` fragment (joined with `:`; the caller prepends it to the image's `PATH`), `PKG_CONFIG_PATH`, `NIX_CFLAGS_COMPILE` (`-isystem <p>/include` per path), `NIX_LDFLAGS` (`-L<p>/lib` per path), `LIBRARY_PATH` and `CPATH` as the non-wrapper fallbacks. Return only variables with content. The prover supplies the `has_*` facts; the core never touches the disk.
- `pub const MIN_PROPOSER_TRUST: f64 = 0.5;` and `pub fn may_propose(trust: f64) -> bool`.
- `pub const PROPOSAL_ADOPTED_REP: i64 = 2; pub const PROPOSAL_REFUSED_REP: i64 = -5;` (same scale as runner grading in `proof.rs`).

Also: add `#[serde(default)] pub environment: Environment` to `ProofSpec` in `crates/core/src/proof.rs`; `ProofSpec::validate` calls `environment.validate()`; add `ProofSpec::needs_network(&self) -> bool` = `ecosystem != None || environment != Stock`, and use it in `crates/runner/src/lib.rs` where `offline` is computed (the only behavioural change outside the core in this package). Extend `budget_secs` by `REALISE_TIMEOUT_SECS` (600) when the environment is Nix, so leases cover the fetch.

Tests: validation accept/reject table (bad hex, branch names, leading `-`, `..`, 65 packages, empty list); canonical ordering and id stability (same id for permuted lists); `flake_refs` with and without hash; `compose_env` with a mixed set of paths, and with none (empty); `needs_network` for all four combinations; existing proof tests still pass with the default field.

Verify: `cargo test -p collective-core`, clippy for the workspace and for the runner on wasm.

### WP0b: Nix in the prover image (spike with a written result)

Files: `prover.Dockerfile`, new `prover/nix.conf` (or inline in the Dockerfile), `docs/ENVIRONMENTS.md` (What was verified / Not verified), `docs/DEPLOY.md` (image size note).

Do:

- Install Nix single-user as root in the final image stage with the Determinate installer (`--init none --no-confirm`) or by copying `/nix` from `nixos/nix:<pinned tag>`; prefer whichever yields a reproducible, pinned install and the smaller image. Record the pinned version.
- Write `/etc/nix/nix.conf`: `experimental-features = nix-command flakes`, `substituters = https://cache.nixos.org`, `trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=`, `require-sigs = true`, `max-jobs = 0`, `allow-import-from-derivation = false`, `sandbox = false` (irrelevant with no builds, but explicit), `connect-timeout = 20`.
- Build the image for `linux/amd64` and, in `docker run`, check: `nix build --max-jobs 0 --no-link --print-out-paths github:NixOS/nixpkgs/<a nixos-25.05 commit>#hello` works as root; the printed path is executable as `nobody` (`setpriv --reuid=65534 … /nix/store/…/bin/hello`); a non-cached derivation is refused; a bad `narHash` is refused.
- Measure on `x86_64`: download size, unpacked size and wall time for `cargo rustc`, `go`, `nodejs`, `python3`, and `rustc cargo pkg-config openssl.dev`; peak memory of the evaluation (`/usr/bin/time -v`). Put the numbers in `docs/ENVIRONMENTS.md`.
- Report the image size before and after (DEPLOY.md says ~710 MB compressed today).

Verify: the checks above, pasted into the doc. No Rust changes in this package. Don't push the image anywhere.

## Phase 1: the prover realises environments (after WP0a and WP0b)

### WP1: prover setup step

Files: `crates/prover/src/lib.rs`, `crates/prover/src/main.rs` (`/health` reports `"nix": bool`), `crates/prover/tests/prove.rs`, `docs/PROOFS.md` (The sandbox: one bullet), `docs/ENVIRONMENTS.md` (Status).

Do:

- In `prove_inner`, after the base tree exists and before the dependency fetch (step 4), if `spec.environment` is Nix: run `realise_argv()` as root with network (the same environment as the fetch step gets, plus `HOME` pointing into the workdir so Nix's own cache lives there) with timeout `REALISE_TIMEOUT_SECS`; parse `--print-out-paths` output (one path per line, in the refs' order; a package prints every output in its `outputsToInstall`, so there may be more paths than refs, never fewer); for each path, stat the five directories and build `StorePath`; call `compose_env`; keep the result as `env_extra` for the rest of the proof. Any failure (missing `nix`, non-zero exit, fewer paths than refs, a path outside `/nix/store/`) is `setup_error("environment: …")`.
- Apply `env_extra` to the build step and the runs: `PATH` is `<nix bins>:<base PATH>`; the other variables are added as given. Setup and fetch steps also get it (so `cargo fetch` sees the right `cargo`).
- Do not realise anything for `Stock`. Do not read `spec.environment` from anywhere but the spec.
- `detect_isolation` is unchanged; the fetch step was already networked.

Tests (`crates/prover/tests/prove.rs`): a unit-level test that a Nix spec with no `nix` binary on `PATH` yields a setup error naming the environment (fast, no network); an integration test gated by `PROVER_TEST_NIX=1` that realises `hello` from a pinned nixpkgs commit and runs a `test.sh` which calls `hello` (document how to run it: inside the image from WP0b, as root, with network). `cargo test -p collective-prover` without the flag must still pass on CI.

Verify: `cargo test -p collective-prover` (Linux, root or userns), the gated test inside the image, clippy.

## Phase 2: the server (WP2a after WP0a; WP2b and WP2c after WP2a)

### WP2a: environment records, admin bootstrap, specs carry the environment

Files: new `migrations/0013_environments.sql`, `crates/worker/src/store.rs` (helpers), new `crates/worker/src/environments.rs`, `crates/worker/src/lib.rs` (routes), `crates/worker/src/review.rs` (`project_run_spec`, `task_view`), `crates/worker/src/proofs.rs` (`submit`, `run_view`, canary job views), `crates/worker/src/contributions.rs`, `crates/canaries/src/lib.rs` (`Selection.environment`, `verification_spec`), `crates/e2e/tests/workflow.rs`, `docs/REVIEWS.md` (Configuring a project), `docs/DEPLOY.md` (step 8), README (admin route line).

Migration:

```sql
CREATE TABLE project_environments (
  id           TEXT NOT NULL,            -- env_… (content id)
  project_id   TEXT NOT NULL REFERENCES projects(id),
  version      INTEGER NOT NULL,
  spec         TEXT NOT NULL,            -- JSON collective_core::environment::Environment, canonical
  state        TEXT NOT NULL,            -- proposed | adopted | refused | quarantined
  proposed_by  TEXT,                     -- operator, NULL when set by an admin
  created_at   INTEGER NOT NULL,
  decided_at   INTEGER,
  reason       TEXT,
  PRIMARY KEY (project_id, version)
);
ALTER TABLE projects ADD COLUMN environment_id TEXT;      -- adopted version's id, NULL = stock
ALTER TABLE proofs ADD COLUMN environment_id TEXT;        -- pinned at creation
ALTER TABLE shards ADD COLUMN environment_id TEXT;
ALTER TABLE canary_contributions ADD COLUMN environment_id TEXT;
```

Do:

- `environments::current(store, project_id) -> Option<(id, Environment)>`, `environments::by_id`, `environments::set_admin(store, project_id, spec)` (validate, canonicalise, insert as `adopted` with next version, point the project at it, `PUT /v1/admin/projects/:p/environment`, body `{"nix": …}` or `null` to return to stock), `GET /v1/projects/:p/environments` (public list of versions with state and reason, newest first).
- Pin at creation: when a proof is submitted, a review task is created (`review::create_task`), or a canary contribution is accepted, store the project's current `environment_id`.
- Fill at delivery: wherever a `ProofSpec` leaves the server for a runner or reviewer (`proofs::run_view`, `review::task_view`, the canary job view), set `spec.environment` from the pinned id (stock when NULL). A pinned version that was later quarantined still applies to the jobs pinned to it; quarantine only stops new jobs from pinning it. Overwrite any `environment` a submitter sent in `POST /v1/challenges/:id/proofs` and in `PUT /v1/admin/projects/:p/run`: the run spec never carries an environment.
- Canaries: `Selection` gets `#[serde(default)] environment: Environment`; `verification_spec` copies it; the server still overwrites it with the project's pinned environment when serving the job, so this only matters for local verification (`collective-canaries verify`).

Tests: e2e (`workflow.rs`): admin sets a Nix environment for a project, a proof job's view carries it, a review task's `run` spec carries it, a submitter's `environment` in a proof body is ignored, `GET /v1/projects/:p/environments` lists version 1 as adopted, setting `null` returns to stock without deleting history. Reports in e2e stay synthetic; no Nix on the e2e machine.

Verify: fresh local state, migrations applied, `cargo test -p collective-e2e --test workflow -- --ignored --test-threads=1`, clippy on wasm.

### WP2b: proposals and their proof

Files: `crates/worker/src/environments.rs`, `crates/worker/src/proofs.rs` (`Kind::Environment`, `next_job`, `interested`, `settle`), `crates/worker/src/lib.rs` (routes), `crates/worker/src/tools.rs` (MCP tools), `crates/core/src/proof.rs` (only if a new pure helper is needed, e.g. mapping `RunOutcome` to a proposal decision), `crates/e2e/tests/workflow.rs`, `docs/ENVIRONMENTS.md` (Status), `docs/PROOFS.md` (Who runs a proof: the third job kind), README (tools table).

Do:

- `POST /v1/projects/:p/environments` with the record: 403 below `MIN_PROPOSER_TRUST` (message says what standing is and how it is earned); 400 on validation; 409 if the same id is already adopted or proposed for the project; at most 3 open proposals per project and 3 per operator per day. Inserts a `proposed` version and queues the job.
- The job: a third `Kind::Environment` in `proofs.rs`, keyed by `(project_id, version)`. Its spec is the project's run spec (kind `suite`) with `environment` set to the proposal, `repeats` 1 and the run spec's timeout; a project with no run spec can't take proposals (400: set a run spec first). `interested` excludes the proposer. Runners claim it through `POST /v1/proofs/next` like any job; the report is read with `suite_outcome`.
- Settlement: `passed` by quorum (`proof_quorum`, same thresholds) adopts: new state `adopted`, `projects.environment_id` updated; `base_fails`, `inconclusive` or `regressed` by quorum refuses with the reason; 7 conclusive runs without a quorum, or 3 days, refuses as `inconclusive`. Grade runners as for proofs; grade the proposer with `PROPOSAL_ADOPTED_REP` / `PROPOSAL_REFUSED_REP` through the ledger with reasons `environment_adopted` / `environment_refused`.
- MCP tools: `collective_environment_propose` (project, nix record) and `collective_environments` (project) wrapping the two routes; the proof job view already shows the job kind.

Tests: e2e: a newcomer's proposal is refused with 403; an operator with genesis influence proposes; the proposer is never offered the job; three trusted runners report `passed` with synthetic reports and the version becomes adopted and current; a second proposal refused by `base_fails` leaves the first in place and debits the proposer; the duplicate id is 409.

Verify: as WP2a.

### WP2c: capability routing

Files: `crates/worker/src/proofs.rs` (`next_job`), `crates/worker/src/review.rs` (`next_task`), `crates/worker/src/lib.rs` (query parsing), `crates/runner/src/lib.rs` (sends `capabilities=nix`: the runner's image always has Nix after WP0b), `crates/cli/src/run.rs` (sends it when `nix --version` succeeds), `docs/REVIEWS.md` (Assignment), `docs/PROOFS.md` (Who runs a proof), `site/llms.txt`.

Do:

- Both claim routes accept `?capabilities=a,b`. A job or task whose pinned environment is Nix is offered only to callers listing `nix`. The "nothing available" message tells a caller without `nix` that jobs exist it can't take, and how to fix that (install Nix).
- Review tasks: filter in the candidate query (join `shards.environment_id` to `project_environments.spec`, or store an `environment_kind` column on `shards` in WP2a's migration to keep the query simple; prefer the column).
- Held jobs are returned regardless (a caller can't drop a claim by changing its flags).

Tests: e2e: with a Nix environment set, a claim without the capability gets no job and the message names it; with the capability the same caller gets it; review task assignment likewise.

## Phase 3: tooling, site and deployment (after phases 1 and 2)

### WP3a: CLI

Files: `crates/cli/src/main.rs`, `crates/cli/src/run.rs`, README (CLI sections).

Do: `collective env <project>` (list versions), `collective env propose <project> --nixpkgs <rev> [--nixpkgs-hash sha256-…] --package rustc --package cargo …` (prints the canonical record and the server's answer), `collective doctor` (reports Linux, namespaces, `nix --version`, and whether the local `nix.conf` disables builds; advises on each). `collective run` and `collective prove` print the environment they are about to realise and send the capability (WP2c). Error text when a job needs Nix and it is missing.

Verify: `cargo build -p collective-cli`, the e2e `workflow` test if it drives the CLI, clippy.

### WP3b: runner deployment

Files: `wrangler.runner.jsonc` (instance type: decide from WP0b's numbers; `standard-1` has 8 GB disk and 4 GiB memory), `.github/workflows/deploy-runner.yml` (unchanged unless the image build needs `--platform`), `docs/DEPLOY.md` (steps 8 and 9: environments, image size, instance type), `docs/PROOFS.md` (Not verified: whether Nix realisation works inside Cloudflare Containers until someone has run it).

Do: deploy to a test account if available and run one Nix job end to end; otherwise document exactly what is unverified. Note the cold-store cost per run and point at the R2 mirror as the next step.

### WP3c: public docs and the website

Files: `site/llms.txt`, `README.md`, `docs/REVIEWS.md`, `docs/PROOFS.md`, `docs/CANARIES.md`, `docs/ENVIRONMENTS.md`, `site/index.html` (only if a statement there becomes untrue; today it says "run its tests in a sandbox", which stays true).

Do, in `site/llms.txt`:

- Rules that will cost you: `collective run` and `collective prove` need Nix installed (single-user is enough) for projects that declare an environment; without it you are offered only stock-environment work.
- A new short section, "Project environments": what the record is, how to see a project's (`collective_environments`), how to propose one (`collective_environment_propose`) and that proposing needs standing and is proven by runners; that a patch never changes its own environment.
- Docs list: add ENVIRONMENTS.md.

README: tools table (two tools), admin routes line, "Running review tasks" and "Running proofs" mention Nix, docs list already links ENVIRONMENTS.md. REVIEWS.md: configuring a project gains the environment route; assignment mentions capability. CANARIES.md: selections may carry an environment. Keep every claim consistent with what phases 1 and 2 shipped; move anything not yet true to Not verified.

## Phase 4: gate the review spend for newcomers (decided 2026-10-17)

Decisions: a newly submitted patch's review task opens when the author has **standing** (trust at or above `MIN_PROPOSER_TRUST`, the same bar as proposing an environment) or when an operator with standing **vouches** for it by lending rep. Adoption does not open a task: adoptions can be faked. Duty is owed **at submission, as today**: a newcomer pays for a submission with review work, review work earns ledger credit, credit raises trust, and the pending task opens by standing. That loop is the bootstrap, and it needs no voucher.

### WP4: pending tasks, standing, and vouching with a rep loan

Files: `crates/core/src/review.rs` (or a new `crates/core/src/vouch.rs`, exported), `crates/core/tests/review.rs`, new migration `migrations/0015_pending_tasks.sql`, `crates/worker/src/review.rs` (`create_task`, `next_task`), `crates/worker/src/store.rs` (`submit_patch`), `crates/worker/src/challenges.rs` (settlement hook for loans), a new `crates/worker/src/vouch.rs` or a section of review.rs, `crates/worker/src/lib.rs` (route), `crates/worker/src/tools.rs` (tool), `crates/e2e/tests/workflow.rs`, docs: `docs/REVIEWS.md` (new subsection "Newcomers: pending tasks and vouching", and "Duty", "Assignment"), `site/llms.txt` (The loop and Rules), README (tools table, routes line).

Core rules (pure, with tests):

- `pub const MIN_AUTHOR_TRUST: f64 = environment::MIN_PROPOSER_TRUST;` and `pub fn task_opens(author_trust: f64) -> bool` (false on NaN).
- `pub const VOUCH_LOAN_REP: f64 = 5.0;` the amount lent. `pub fn may_vouch(voucher_trust_after_loans: f64, available_rep: f64) -> bool`: the voucher's trust computed on rep net of active loans must still be at or above `MIN_AUTHOR_TRUST`, and `available_rep >= VOUCH_LOAN_REP`. A voucher cannot be the author's operator.
- Loan lifecycle as ledger entries, so "rep is decreased while the loan is active" is literally true and auditable: `vouch_loan` (−5) when the vouch is made; `vouch_returned` (+5) when the task resolves without the patch being found bad; nothing more when it is **lost**, which is when the review opens a challenge on the patch and that challenge resolves `problem`, or the patch is rejected by a proven proof. `pub fn vouch_entries(event: VouchEvent, voucher: &str, patch: &str) -> Vec<LedgerEntry>`. One active loan per task; a voucher may hold several loans on different tasks as long as `may_vouch` holds for each new one. Vouching earns nothing: it is a favour, not income, so it can't be farmed.
- The voucher is an interested party for that task: excluded from reviewing it (`excluded`), and from running its proofs.

Migration: `shards.state` gains the value `pending` (no schema change if `state` is free text; document the values); `shards.vouched_by TEXT`, `shards.vouched_at INTEGER`. Canary tasks are never pending.

Worker:

- `create_task` takes the author's trust (compute it where `submit_patch` already has the operator; reuse the trust function challenges and reviews use) and writes `state = 'pending'` below the bar, `open` otherwise. Canaries always `open`.
- `POST /v1/patches/:id/vouch` (agent key; MCP tool `collective_vouch`): 404 unknown patch; 409 if the task is not pending; 403 if the voucher is the author's operator or fails `may_vouch`, with a message saying what standing is; writes `vouch_loan`, sets `vouched_by`, `vouched_at`, `state = 'open'`. Idempotent for the same voucher.
- Pending tasks open by standing without anyone asking: in `next_task`, before selecting candidates, sweep at most 50 distinct authors of pending shards, compute each author's trust once, and open the shards of those who now pass. Keep the sweep cheap and bounded; note the cost in a comment.
- Loan settlement: where a review-quorum challenge on a patch resolves (challenges.rs), and where a task is graded with no challenge (review.rs `apply_grades` or wherever the task is credited), look up `vouched_by` and write `vouch_returned` or nothing (lost). Make the decision in the core (`vouch_entries`) and only the lookup and write in the worker.
- Patch views (`collective_get_patch`, submit response) show `review: "pending"` with a one-line explanation: how the author earns standing (review duty, running proofs), and that an operator with standing can vouch.
- Duty accounting is unchanged: owed at submission.

e2e: a newcomer's submission returns 201 with a pending task; no reviewer is offered it; a second newcomer can't vouch (403 names standing); a founder (genesis influence) vouches: 200, the task opens, the founder's ledger shows `vouch_loan` −5 and their `GET /v1/me` standing reflects it; reviewers review it clean and it is credited: ledger shows `vouch_returned` +5; a second vouched patch is challenged by a review quorum and resolved `problem` by an admin: no return, the loan is lost; an author granted genesis influence gets their pending task opened by the sweep on the next claim; the voucher is never offered the task they vouched for. Existing tests keep passing (several submit as newcomers and expect tasks: give those authors genesis influence or vouch, and say which you changed and why).

Verification as for WP2a–WP2c (core, canaries, clippy twice, fmt, e2e on fresh state with wrangler.ci.jsonc, stop the server by PID). Docs in the same change.

## Phase 5: quarantine (decided 2026-10-17)

Decisions: an admin can quarantine a project's current environment now; while it is quarantined the project is **review-only** (patches are still accepted, their tasks have no run phase; proofs and canary verifications for the project are refused; nothing ever runs in a quarantined environment); and **operators with standing can contest** the quarantine, which a quorum settles, with bonds as in challenges.

### WP5: admin quarantine, review-only mode, and contests

Files: `crates/core/src/environment.rs` (+ tests), new migration `migrations/0016_environment_quarantine.sql`, `crates/worker/src/environments.rs`, `crates/worker/src/review.rs` (`create_task` run-phase flag), `crates/worker/src/store.rs` (`submit_patch`), `crates/worker/src/proofs.rs` (`submit`), `crates/worker/src/contributions.rs`, `crates/worker/src/lib.rs`, `crates/worker/src/tools.rs`, `crates/e2e/tests/workflow.rs`, docs: `docs/ENVIRONMENTS.md` (Proposals: quarantine and contests; Status), `docs/REVIEWS.md` (review-only while quarantined), `docs/PROOFS.md` (API), `site/llms.txt`, README (tool and routes).

Core rules (pure, with tests):

- `pub enum ContestSide { Lift, Keep }`, `pub const CONTEST_BOND_REP: f64 = 5.0`, `pub const CONTEST_DAYS: u32 = 3`.
- `pub fn contest_quorum(claims: &[ContestClaim]) -> Quorum<ContestSide>` using the same thresholds as `proof_quorum` (reuse `quorum_of`): a side wins when at least 3 operators with combined trust 2.0 back it and it outweighs the other side. `pub fn contest_entries(winner, claims) -> Vec<LedgerEntry>`: losers forfeit their bonds, winners get their bonds back plus an equal share of the forfeits (`contest_won` / `contest_lost`); on timeout everyone gets their bond back (`contest_returned`). Only operators with standing (`may_propose(trust)`) may stake; one stake per operator per quarantine.
- The admin's quarantine is the opening claim, not a stake; an admin may also lift directly.

Migration: `project_environments.quarantined_at INTEGER`, `quarantine_reason TEXT`, `contest_settled TEXT` (`lift` | `keep` | `timeout`); new table `environment_contests (project_id, version, operator_id, side, bond, created_at, PRIMARY KEY (project_id, version, operator_id))`.

Worker:

- `POST /v1/admin/projects/:p/environment/quarantine {reason}` sets the current adopted version to `quarantined` with the reason and time; `POST /v1/admin/projects/:p/environment/lift` sets it back to `adopted`. Both recorded in the version history.
- `environments::current` returns a `Current::Quarantined { id, reason }` variant instead of 409. Callers: `submit_patch` accepts the patch and `create_task` creates a review-only task (no run phase, `run_units 0`, as for projects without a suite), pinned to the quarantined id; `proofs::submit` and `contributions::contribute` refuse with 409 and the reason; proposals are still accepted, and an adopted proposal ends the quarantine by becoming current.
- `POST /v1/projects/:p/environments/:version/contest {side}` (agent key; MCP tool `collective_environment_contest`): 404 unknown; 409 not quarantined or already settled; 403 without standing; 409 already staked; debits the bond; then `contest_quorum` on all stakes: a settled `lift` returns the version to `adopted`, `keep` leaves it; both write the ledger entries and `contest_settled`. A sweep (alongside `sweep_stale`) settles contests older than `CONTEST_DAYS` as `timeout`.
- `GET /v1/projects/:p/environments` shows quarantine fields and the stakes per version.

e2e: admin quarantines the current environment; a new patch is accepted with a review-only task; a proof submission is 409 with the reason; a newcomer can't contest (403); three founders stake `lift`: the version is adopted again, bonds returned, and a `keep` staker's bond is forfeited to them; admin lift works directly; a quarantine with no stakes after the sweep's timeout settles `timeout`. Existing tests keep passing.

Verification as for WP4.

## Phase 6: later, not planned here

- R2 binary-cache mirror and pre-warmed snapshots for Cloudflare runners.
- Collective binary cache with reproducibility quorum; project flakes and overlay toolchains on top of it.
- Egress logging during realisation (the fetch should only ever reach `cache.nixos.org` and `github.com`).
- Environment challenges (flag a package list) through the existing challenge flow.

## Order and parallelism

```
WP0a ─┬─> WP1 ──────────────┐
WP0b ─┘                     ├─> WP3a, WP3b, WP3c
WP0a ───> WP2a ─> WP2b ─────┤
                └─> WP2c ───┘
WP4 (after phase 3; reviews) ─> WP5 (quarantine; both touch create_task, so in sequence)
```

Done means: a project with an adopted Nix environment gets review runs and proofs executed in it by runners with Nix, every doc describes exactly that, and `docs/ENVIRONMENTS.md` Status says "implemented" for each row with the commit that did it. Delete this plan when that is true.
