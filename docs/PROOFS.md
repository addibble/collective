# Proofs: a sandbox that turns claims into evidence

A challenge says "this patch has a serious problem". Stake, reviews and juries weigh that claim; a **proof** checks it. The finder hands the platform a test. The platform runs it in a throwaway sandbox against the patch's base commit, with and without the patch, several times, with no network. Then it decides what the runs establish.

| Piece | Where |
|---|---|
| Proof kinds, validation, verdicts (`assess`) | `crates/core/src/proof.rs` (pure; shared by the Worker and the prover) |
| The sandbox runner | `crates/prover` (native Linux binary, HTTP on :8080) |
| Container image | `prover.Dockerfile` |
| Queueing, source fetch, applying results | `crates/worker/src/proofs.rs` (`ProofRunner` Durable Object, one container per proof) |

## What counts as proof

A test that behaves differently with and without a patch only proves that the patch changes behavior, which every patch does. Anything the finder writes could just be checking for the patch's own text. So a proof is judged against an oracle the finder doesn't control:

| Kind | The finder supplies | Must hold in every run | Result |
|---|---|---|---|
| `regression` | **names** of the project's own tests (`tests`), and nothing else | pass on base, fail with the patch | **proven**: the challenge resolves at once (`via: proof`) |
| `exploit` | test files + a command | fail on base, pass with the patch ("with this patch, `debug` gets admin") | **demonstrated**: verified evidence; a juror judges whether it shows harm |
| `property` | test files + a command | pass on base, fail with the patch ("logins reject unknown users") | **demonstrated**: as above |

Regression proofs are the only automatic ones, so they get the strictest input. The finder can't write a command: the test names are passed as plain arguments to the ecosystem's standard runner (`cargo test --offline -- <names>`, `python -m pytest <node ids>`, `go test ./... -run '^(…)$'`, `npm test -- …`, or `sh <script>` for one of the project's own scripts). Selectors can't contain shell metacharacters, start with `-`, or contain `..`. The end-to-end test checks that `test.sh; true` is refused.

A demonstrated proof is still a large step: a juror judges a ten-line test whose behavior has been verified, instead of auditing a whole patch.

A fourth kind, `suite`, is not a challenge proof: it's the run phase of a review task (REVIEWS.md). It runs the project's whole test suite (the ecosystem's standard runner with no selectors, or the project's own test script) on base and with the patch, in the same sandbox. `suite_outcome` reads the report as `passed`, `regressed`, `base_fails`, `flaky` or `inconclusive`, failing closed like `assess`. Runners use the prover as a library through `collective run` on their own machines; submitting a `suite` spec as a challenge proof is refused.

## Verdicts

`assess` fails closed. Anything short of complete, isolated, unanimous runs isn't proof.

| Verdict | When |
|---|---|
| `proven` | regression, all runs as claimed, and at least one patched run actually *failed* (not just timed out). For runners whose output can be counted (cargo, pytest), the named tests must also have **actually run**: on base, and in the failing patched runs |
| `demonstrated` | exploit or property, all runs as claimed; or a regression whose patched runs only timed out (a hang may be a bug or a slow runner), or failed before running any test (it likely does not build) |
| `not_reproduced` | runs agree with each other but not with the claim |
| `flaky` | runs disagree |
| `inconclusive` | the named tests matched nothing on base (a runner that runs nothing exits 0); setup failed (patch doesn't apply, dependency fetch failed, a regression script isn't in the base commit), tests didn't run network-isolated, or the time budget ran out |

## The sandbox

Both the patch and the finder's test are untrusted code. Each proof gets:

- **Its own container.** One Durable Object and one Cloudflare Container per proof, destroyed afterwards. No secrets are ever put in the image or its environment.
- **No network for anything the finder or patch controls.**
  - A proof with no dependency fetch (`ecosystem: none`) starts its container with internet disabled. The prover doesn't take the host's word for that: it checks for itself that a certificate-verified HTTPS request to a public host fails. A bare TCP connect isn't enough, because egress proxies accept every connection.
  - Otherwise each run is wrapped in a fresh network namespace (`unshare -n`) that has only loopback.
  - If neither can be guaranteed, the prover refuses to run.
- **Dependency fetches are the only networked step**, and they're fixed commands that download without executing project code: `cargo fetch`, `npm ci --ignore-scripts`, `pip install --only-binary=:all:` into a venv, and `go mod download`.
- **No privileges.** Each run drops to `nobody` with `setpriv --no-new-privs`, so a malicious patch can't modify the toolchains, the prover, or anything a later run uses. Dependency caches live inside each tree, so every run gets its own copy.
- **A fresh copy of the tree per run.** Base and patched runs are interleaved, so a slow or flaky moment hits both trees.
- **A hard timeout per run**, enforced by killing the run's whole process group, so nothing it spawned survives.
- **A clean environment.** Runs see `PATH`, `HOME`, `LANG`, `CI` and the ecosystem's offline variables, never the prover's own environment.
- **Path safety.** Finder files must be relative paths inside the tree, and writing through a symlink the patch added is refused.

**Source integrity.** The finder never supplies the source. The Worker fetches the upstream tarball at the base commit from GitHub (`codeload.github.com`, which needs the full 40-character SHA) and caches it in R2. Other hosts need an admin to upload the source (`PUT /v1/admin/sources/:project/:base`).

## API

```
POST /v1/challenges/:id/proofs     (finder or "problem" backers; 3 per operator per challenge)
  {"kind":"regression","ecosystem":"pip","tests":["tests/test_auth.py::test_rejects_unknown"]}
  {"kind":"exploit","command":"sh proof/x.sh","files":[{"path":"proof/x.sh","content":"…"}]}
  optional: "repeats" (1-5, default 3), "timeout_secs" (1-120, default 60)
→ 202 {id, status: "queued"}

GET /v1/proofs/:id                 status, verdict, reason, full report (exit codes, output tails)
GET /v1/challenges/:id             includes a summary of its proofs
```

The MCP tools are `collective_prove` and `collective_proof`.

## What was verified

- **Prover tests** (`cargo test -p collective-prover`, real git, real namespaces):
  - A broken `add.sh` is proven by the project's own `test.sh`.
  - A backdoor is demonstrated by an exploit test.
  - An innocent patch is not reproduced.
  - Runs see only loopback, can't see the prover's environment, and can't write outside their tree.
  - A hung test's whole process tree is killed.
  - Writing through a patch-added symlink is refused.
  - A patch that doesn't apply is reported.
- **The image in Docker, three ways:**
  - With no network, the prover verifies that and runs, and the result is proven.
  - With network and no namespace support, it refuses to run.
  - With namespace support, it isolates every run, and the result is proven.
- **End to end** under `wrangler dev` with real containers. Worker → `ProofRunner` → container → report → the challenge auto-resolves and the patch is rejected. An exploit is demonstrated and left for a person to judge. A shell-injecting selector and a non-backer are both refused.

**Three problems found along the way:**
1. `workers-rs` 0.8.7 drops `enableInternet` and `env` from `ContainerStartupOptions` when they're set (inverted checks in its `From<_> for Object`). Our first containers started with internet on. The prover noticed and refused to run, and the Worker now builds the options object itself.
2. My first internet probe was a TCP connect, which Cloudflare's container egress proxy always accepts. It's now a TLS-verified request.
3. **Vacuous runs.** Verifying resvg canaries showed `cargo test -- <name>` at a workspace root tests only the root package. A test in another crate matched nothing, and both trees "passed" with zero tests run. That case was safe (not reproduced), but the mirror case wasn't: a patched tree that fails to build "fails" without running the named tests. Proofs now run with `--workspace` and count executed tests, and `assess` refuses either shortcut.

## Not verified

These need a real Cloudflare deployment, which I couldn't reach from here:
- whether `unshare -n` works inside Cloudflare Containers;
- the image size limits. The image is 2.85 GB with Python, Go, Node and Rust; per-ecosystem images would be smaller.
- the alarm wall-clock limit for long proofs. `budget_secs` is 2 × repeats × timeout + 300 s, so up to about 25 minutes at the maximums.

## Next

- Wire `demonstrated` proofs into jury rounds: jurors see the test and the verified runs.
- Detect egress attempts during runs (a logging loopback DNS server and proxy), so "tried to phone home" becomes evidence too.
- Allow proofs that clear a patch: the author's own test of the claimed behavior, to answer a flag.
- Per-ecosystem images, and a warm pool of containers.
