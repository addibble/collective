# Canary selections

**Production selections must not live in this public repository.** A canary only works if reviewers can't recognize it. A public list of the upstream commits that canaries revert lets anyone check a review task's base commit against the list and spot every canary. Keep real selections, and the generated `.jsonl` files, in private storage (a private repository, or an encrypted bucket only the operators of the deployment can read).

`examples/resvg.json` is a worked example and a test fixture. Because it's public, its ten commits must never be used as live canaries. Verification results for it (2026-10-12, sandbox with test counting):

| Commit | Kind | Verdict |
|---|---|---|
| 6bc52783 | robustness fix | proven |
| ded4b139 | robustness fix | proven |
| 42145621 | robustness fix | proven |
| b12646b2 | feature | proven |
| 6debc440 | feature | proven |
| a53573b7 | feature | proven |
| e2f11fe7 | robustness fix | demonstrated: the revert no longer builds |
| 1505e4b6 | feature | demonstrated: the revert no longer builds (the commit also bumped a dependency) |
| 4a08e340 | robustness fix | not reproduced |
| fc4d020a | feature | not reproduced |
