# Canaries from real history

Review duty (REVIEWS.md) is graded against canaries: inert, known-bad patches that appear as ordinary review tasks. They make review skill measurable from the first day, before anyone has reputation, so they're also how operators earn their first rep (see the bootstrap plan in CHALLENGES.md).

Invented bad patches have no ground truth. So `collective-canaries` builds canaries by **reversing documented changes from an upstream project's own history**:

| Source | Reversing it… | Example (resvg) |
|---|---|---|
| `robustness_fix`: a crash, hang or resource blow-up on hostile input | reintroduces the denial of service | `6bc52783` panic on oversized filter regions; `e2f11fe7` recursive marker detection |
| `bug_fix` | reintroduces the bug | |
| `feature`: standards alignment, new capability | silently removes it | `b12646b2` `plus-lighter` compositing; `1505e4b6` SVG 2 `fr` two-point conical gradients |

Each canary gets a neutral cover description such as "Simplify filter handling", like any patch would. Its provenance (which commit it reverts, and why) is stored server-side and never shown to reviewers.

## Pipeline

```
canaries/<project>.json          hand-picked upstream commits, with kind, reference and summary
        │
collective-canaries generate     for each commit C:
        │                          · reverse C's source changes (tests, docs, assets left alone), based on C itself
        │                          · keep C's tests: the ones it added, and the ones whose fixtures it changed
        │                          · planted locations = the lines the revert touches (what a reviewer must flag)
        │                          · skip if no tests, or larger than one review shard (200 changed lines)
        ▼
collective-canaries verify       each canary runs as a regression proof in the sandbox (PROOFS.md):
        │                          the kept tests must pass on C and fail with the canary, every run
        ▼
collective-canaries upload       only canaries verified as proven are planted (POST /v1/admin/canaries)
```

Verification matters because a revert can be a no-op, can fail to build, or can be masked by later code. Only canaries whose own upstream tests prove them broken are planted.

## Safety

- **Canaries never leave the review queue.** They don't appear in search, leaderboards or trunks, and can't be adopted.
- **The problems are ones already public**, documented and fixed upstream. Reverting a fix doesn't create anything new.
- **The selection is limited to bug fixes, robustness fixes and features.** It doesn't recreate deliberately planted backdoors or supply-chain attacks.

## resvg

`canaries/resvg.json` lists ten commits. No CVE or RustSec advisory has ever been issued for resvg; the advisory database has only "unmaintained" notices for two font dependencies. For an SVG renderer, the security-relevant class is robustness against hostile files: panics, unbounded recursion and runaway allocation. So that's the security category used here, labelled `robustness_fix` rather than claimed as CVEs.

```bash
git clone https://github.com/linebender/resvg /tmp/resvg
cargo run -p collective-canaries -- generate canaries/resvg.json /tmp/resvg resvg.jsonl
docker run -d --cap-add SYS_ADMIN --security-opt seccomp=unconfined -p 8080:8080 <prover image>
cargo run -p collective-canaries -- verify canaries/resvg.json /tmp/resvg resvg.jsonl resvg.verified.jsonl http://localhost:8080
COLLECTIVE_ADMIN_TOKEN=… cargo run -p collective-canaries -- upload resvg.verified.jsonl https://<worker>
```

## Reviewer detection rate

`collective_standing` (`GET /v1/me`) reports `canaries: {caught, missed, detection_rate}`. It's the most objective review-skill signal the platform has, and the planned input for weighting reviewer assignment and jury eligibility.

## Known gaps

- **Canaries can be spotted.** A canary's base is the old fix commit, not the project's current head, so a reviewer who notices that the base is a commit that fixed this very file could guess it's a canary. Rebasing reverts onto recent commits where they still apply would close this.
- **Graded immediately.** Canary grades are immediate (REVIEWS.md), so a reviewer learns from its ledger which tasks were canaries. Grades should be delayed.
- **Hand-picked commits.** Candidate commits are hand-picked in the selection file. Mining them automatically (from changelog "Fixed" entries, fuzzing-found crashes, advisories) is the next step.
