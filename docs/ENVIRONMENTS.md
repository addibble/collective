# Environments: what a proof runs in

A proof (PROOFS.md) and a review run (REVIEWS.md) execute a project's tests in a sandbox. Today the sandbox is one image, `prover.Dockerfile`, with Rust, Go, Node and Python, and a project's needs are described only by its `ecosystem`, which names a package manager. That covers pure-language projects and nothing else: a suite that needs a system library, a particular toolchain version or a database can't run, and every such proof comes back `inconclusive`.

The obvious fix, letting a project ship a Dockerfile, is the one thing this design refuses. A Dockerfile is a **build program**: assembling it runs arbitrary `RUN` steps with network access, the result differs from one build to the next, and someone has to pay for every build. The platform runs no proofs, so it must not build images either. And a protocol that settles by agreement between parties who don't trust each other can't use evidence that doesn't repeat: three honest runners building the same Dockerfile at different times get different packages, disagree, and fail closed.

## Decision

**A project's environment is a Nix package set, realised by substitution only.** A record names a `nixpkgs` commit and a list of attribute names. The prover fetches exactly those store paths, signed, from `cache.nixos.org`, with local builds disabled, and composes the run's environment from them. Nothing is built, nothing is reviewed but attribute names, and every runner gets the same bytes.

Other mechanisms were considered and set aside (below). Attested container images in particular were specified in an earlier draft of this document and dropped: not because they aren't repeatable (a digest is), but because they rarely apply, can't be read, and need a prover that runs inside foreign images.

## Principles

- **A declaration, never a program.** An environment is data with no commands in it. Nobody in the collective builds anything on anyone's behalf.
- **The environment is a project record, not part of the patch.** It is stored by the server, versioned, and changed by proposals (below). The prover ignores any `flake.nix`, `shell.nix` or Dockerfile in the tree. A patch can never change the environment it is tested in.
- **Every job pins its environment.** A job carries the environment version it was created under, so every runner's report describes the same thing.
- **Runners pay, and runners say what they can serve.** A runner that has Nix is offered jobs that need it; one that doesn't isn't. A project with a large environment gets slower quorums. Nothing is built or paid for centrally.
- **Fail closed.** A store path that doesn't substitute, a signature that doesn't verify, a revision that doesn't fetch: the setup fails and the run is `inconclusive`, exactly as a failed `cargo fetch` is today.

## Status

| Piece | State |
|---|---|
| Records and pinning | Implemented. A record is validated and stored canonical with a content id (`crates/core/src/environment.rs`). An admin sets a project's environment (`PUT /v1/admin/projects/:project/environment`, `null` for the stock image; REVIEWS.md), every change is a new version, and the history is public (`GET /v1/projects/:project/environments`, `collective_environments`, `collective env`). Every proof, review task, planted canary and contributed canary pins the version current at creation, and the server writes the pinned record into the spec whenever it hands one out; an `environment` sent in a proof or a run spec is ignored. e2e test `environments_are_pinned_and_filled_by_the_server`. |
| Proposals | Implemented. An operator with standing proposes (`POST /v1/projects/:project/environments`, `collective_environment_propose`, `collective env propose`); the proposed version is a third kind of proof job that runners other than the proposer claim, and a quorum adopts or refuses it and grades the proposer (Proposals). e2e test `environments_are_proposed_with_standing_and_proven_by_runners`, with synthetic reports. |
| Runner capabilities | Implemented. Both claim routes take `?capabilities=nix`, and `collective_review_next` a `capabilities` array; work pinned to a Nix environment, and every proposal's job, goes only to callers listing `nix` (Runners). The Cloudflare runner always sends it; `collective run` and `collective prove` send it when `nix --version` works. Covered by both e2e tests above. |
| Substitute-only Nix in the prover | Implemented. The prover image has Nix 2.35.3, single-user, with `/etc/nix/nix.conf` as below. When a spec names a Nix environment, the prover realises it before the dependency fetch and applies it to the fetch, the builds and every run (Realising it). Prover tests `a_nix_environment_without_nix_is_inconclusive`, `a_realisation_that_does_not_add_up_is_refused`, `an_empty_patch_runs_the_suite_on_the_base_twice`, and `realises_a_nix_environment_and_runs_in_it`, which needs `PROVER_TEST_NIX=1` and ran in the image under `linux/amd64` emulation (What was verified). Never run inside a Cloudflare Container (Not verified). |
| `collective doctor` | Implemented: checks a machine for Linux, network namespaces, `nix --version`, a `nix.conf` that disables builds, and `git` (README). |
| Challenging an environment | Not implemented (Later). |

## The record

```json
{
  "nix": {
    "nixpkgs": "<40-character commit of NixOS/nixpkgs>",
    "nixpkgs_hash": "sha256-…",
    "packages": ["rustc", "cargo", "pkg-config", "openssl.dev", "sqlite.dev"]
  }
}
```

- `nixpkgs` is a commit, never a branch or channel, so the same record means the same thing forever. The server doesn't check which branch the commit is on; accepting only commits on a release branch (`nixos-25.05`, `nixos-unstable` at a date), so the set of revisions in use stays small enough to mirror, is for later.
- `nixpkgs_hash`, optional, is the NAR hash of that tree, which makes the fetch of nixpkgs itself verified rather than trusted to GitHub (`github:NixOS/nixpkgs/<rev>?narHash=…`). The proposer gets it from `nix flake metadata`.
- `packages` are attribute paths under that nixpkgs, in a strict grammar (letters, digits, `_`, `-`, `.`; no leading `-` or `.`, no empty segment between dots), 1 to 64 of them, each at most 128 characters. They are sorted and de-duplicated, and the record's id (`env_` and 32 hex characters) is the SHA-256 of its canonical form, so identical proposals are one proposal. Unknown fields are refused.

The record is orthogonal to `ecosystem`. Nix provides the system layer: toolchains, compilers, C libraries, tools. `cargo fetch`, `npm ci` and the rest still fetch the project's own dependencies as they do now, and the stock prover image remains the environment for projects that declare none.

## Realising it

In the prover, after the source is unpacked and before the dependency fetch, as a networked setup step with its own timeout:

```
nix build --max-jobs 0 --no-link --print-out-paths \
    --option allow-import-from-derivation false \
    github:NixOS/nixpkgs/<rev>?narHash=<hash>#<pkg> …
```

with `nix.conf` in the image pinning `substituters = https://cache.nixos.org`, its public key, `require-sigs = true` and `max-jobs = 0`. Then the prover composes the run's environment from the store paths it got back:

- `PATH`: each package's `bin`, ahead of the image's own.
- `PKG_CONFIG_PATH`: each `lib/pkgconfig` and `share/pkgconfig`.
- `NIX_CFLAGS_COMPILE` and `NIX_LDFLAGS`, which the nixpkgs compiler wrapper reads: `-isystem <pkg>/include` and `-L<pkg>/lib`.
- `LIBRARY_PATH` and `CPATH`: the same `lib` and `include` directories, for a compiler that isn't the nixpkgs wrapper.

The prover reads the printed paths in order and checks that each exists. A package prints one path per output it installs (`openssl` prints its `bin` and `man`, `rustc` its `man` and `out`), so there may be more paths than packages but never fewer: fewer, or any path outside `/nix/store/`, fails the setup. The core composes the variables from what the prover found in each path (`compose_env`); the prover gathers the facts. Nix runs as the prover's user with `HOME` inside the work directory, so its per-user caches and flake registry go with it; the store is the only thing that persists.

That is a small, deterministic slice of what nixpkgs' `stdenv` setup hooks do in a dev shell, done by the prover rather than Nix so that nothing is built. `nix shell` sets only `PATH`, and `nix develop` builds a derivation locally, which is exactly what `max-jobs = 0` forbids.

Runs then proceed as today: the drop to `nobody`, the network namespace, the fresh tree per run. `/nix/store` is root-owned and world-readable, so a run can use it and can't change it.

## Proposals: how an environment is adopted

The free resource here is runner compute: a proposed environment is proven by a quorum of runners each running the project's suite in it, so one proposal costs three operators minutes of CPU and a few hundred megabytes of fetching. The gate is on that spend, in the shape GitHub uses for first-time contributors: the first request costs standing, and after that the proposer has standing to lose.

- **Proposing needs standing.** An operator below a trust threshold (`MIN_PROPOSER_TRUST` in the core) can't propose. Newcomers start at 0.25 and earn the rest by adopted patches, credited reviews and runs. Genesis influence counts.
- **The proposal is small and readable**: a commit and attribute names, public as soon as it is made (`GET /v1/projects/:project/environments`).
- **The first job in a proposed environment is the proof.** The proposal needs the project to have a run spec and a base commit (400 otherwise). The server stores it as a `proposed` version, with the run spec as it is at that moment, and that version is a job (`<project id>:<version>`) for runners: a `suite` run of the project's test suite, once, in the proposed environment, on a base fixed at proposal time (the project's default base, else its newest patch's base), with nothing applied. Runners claim it like a canary verification, and the proposer never can. Each report is read with `suite_outcome`, and the thresholds are `proof_quorum`'s (`outcome_quorum` in the core): it is adopted when a quorum reports `passed`, and refused when a quorum reports anything else, `base_fails`, `regressed`, `flaky` or `inconclusive`. Unlike a proof, an inconclusive run is a vote here, because an environment that can't be realised makes every run inconclusive. 7 runs without a quorum, or 3 days, refuse it as inconclusive with nobody graded. This reuses the proof machinery, and it is what stops a wrong toolchain version or a missing library from ever becoming the environment.
- **Limits.** The same record can't be proposed while it is open or current (409). A project has at most 3 open proposals, and an operator makes at most 3 a day (429).
- **The proposer is graded like a runner.** Adopted: a small credit (`PROPOSAL_ADOPTED_REP`, 2, ledger reason `environment_adopted`). Refused by a quorum: a larger debit (`PROPOSAL_REFUSED_REP`, -5, `environment_refused`). Runners are graded as for a proof. Standing is something to lose, and trust decays, so standing earned once isn't a permanent pass.
- **Versioned, never edited.** Each proposal and each admin change is a new version; adoption changes a version's state and points the project at it. Jobs already created keep the version they pinned. A project's environment history is public.
- **Bootstrap.** An admin can set a project's environment directly, as the run spec is set today, until there are operators with standing to propose.

The same gate would fit the other place where a newcomer's request spends other operators' compute, a newly submitted patch's review task. That isn't built (Later).

## Runners

- **Capability, not catalogue.** Any runner with Nix can realise any environment, so a runner advertises one capability, `nix`, when it asks for work: `?capabilities=nix` on `POST /v1/proofs/next` and `POST /v1/reviews/next`, or `capabilities: ["nix"]` in `collective_review_next`. Other names are ignored. A proof or canary job pinned to a Nix environment, and every proposal's job, is offered only to callers that list it; so is a review task pinned to one, since the run phase follows the review. Callers without it keep getting stock-image work, and one left without any for lack of Nix is told so and how to fix it. A held job or task comes back whatever the caller sends, so a claim can't be dropped by changing flags.
- **The prover image carries Nix**, single-user, as root, no daemon. Builds are disabled, so Nix's own sandbox, which needs root or user namespaces, is never used.
- **Cold stores on Cloudflare.** A Cloudflare container is new per run, so each run downloads its environment: about 300 MiB for a Rust toolchain (299 MiB, 1.4 GiB unpacked, for `rustc cargo pkg-config openssl.dev`; measured below). The Cloudflare runner uses the `standard-2` instance type (12 GB disk) for this; the reasoning and what is unverified are in DEPLOY.md, step 9. Machine runners keep a warm store across jobs. Two ways to make Cloudflare cheap, neither built yet and neither adding trust: an R2 bucket as a binary cache mirroring the paths adopted environments need (the signatures stay `cache.nixos.org`'s, so the mirror is a cache, not an authority), and pre-warmed container snapshots under the `durable_object` scheduling policy.

## Limits

- **Version pinning.** A nixpkgs commit has one `rustc`, one `go`, a few `nodejs` and `python` versions. Exact toolchain versions come from overlays such as rust-overlay or fenix, which are flakes with their own caches: that needs a collective binary cache first (below).
- **Beyond `PATH` and pkg-config.** Projects whose builds need a full `stdenv` shell (`cmake` and autotools setups, Python packages with native extensions reading hooks) may not work with prover-composed environments. How far the simple composition goes is an open measurement.
- **Not expressible.** Proprietary SDKs, Windows and macOS. No container mechanism helps with those either.

## Later

Not built:

- **Challenging an environment.** An adopted environment should be flaggable like a patch (a package list that pins a backdoored version). The server already treats a `quarantined` version as stopping new jobs while work pinned to it is still served, but nothing sets that state yet.
- **Release-branch commits only**, so the revisions in use stay few enough to mirror (The record).
- **An R2 mirror and pre-warmed containers** for Cloudflare runners (Runners).
- **Egress logging during realisation**: the fetch should only ever reach `cache.nixos.org` and `github.com`.
- **The same standing gate for a newcomer's review task**: the patch stored, searchable and adoptable at once, its review task opened when the author has standing or the patch gets its first independent adoption (REVIEWS.md would change).

### A collective binary cache

Two things need store paths that `cache.nixos.org` doesn't have: a project's own `devShell` and packages Hydra doesn't build. Both call for a cache signed with a collective key, and the question of who builds for it. The answer is the same as for proofs: a store path becomes substitutable once a quorum of independent runners has built it with the Nix sandbox on and produced the same hash. Environment derivations (`mkShell`, `buildEnv`) are symlink trees and environment dumps, so they reproduce trivially; anything that doesn't reproduce isn't served. R2 is the natural home: S3-compatible for `nix copy`, free to serve.

## Considered and set aside

- **Images the project attested.** Some projects publish an image from CI with a GitHub artifact attestation, a Sigstore bundle tying a digest to a workflow in their repository. Verification works and was exercised by hand (below). But the attested image is almost always the shipped product with no toolchain in it; a gigabyte of filesystem can't be reviewed, where a package list can; and running the prover inside a foreign image needs a static prover with no dependence on `git` or util-linux, plus a per-environment image catalogue on Cloudflare, where images are fixed at deploy time. It may return as a second tier for projects that attest a builder image.
- **Building the project's Dockerfile on the runner**, even from the base commit. Arbitrary execution with network, impossible unprivileged in Containers without Docker-in-Docker, and not reproducible across runners.
- **Images from anyone**, admitted on reputation alone.

## What was verified

In a throwaway `nixos/nix` container (Nix 2.35.3, aarch64, Docker on a laptop):

| Check | Result |
|---|---|
| `nix shell --max-jobs 0 nixpkgs#hello --command hello` against `github:NixOS/nixpkgs/nixos-25.05` | Fetched 5 signed paths (9.5 MiB) from `cache.nixos.org` and ran |
| The same with a derivation not in the cache (`hello.overrideAttrs`) | Refused: `local builds are disabled (max-jobs = 0)` |
| What `nix shell` sets | `PATH` only |
| Closure of `cargo` + `rustc` from nixpkgs 25.05 | 26 paths, 296 MiB to download, 1.5 GiB unpacked, 38 s on this connection |
| `nix develop` | Per the manual it builds a modified derivation to capture the shell's environment: a local build |

For the record, the attested-image checks: `gh attestation verify oci://ghcr.io/astral-sh/uv:latest --owner astral-sh` succeeds; `GET /repos/astral-sh/uv/attestations/sha256:…` returns the v0.3 bundle inline, unauthenticated, for the image index digest and 404 for the `linux/amd64` manifest; `sigstore-verify` 0.15 compiles for `wasm32-unknown-unknown` without default features but then has no crypto backend.

### In the prover image (WP0b)

Install: the `nix` binary and its closure (62 store paths, 121 MB unpacked, about 42 MB compressed) are copied from the pinned `nixos/nix:2.35.3` image (digest `sha256:ff480dbb23511aa1c10710aa068ceea5b93fed9bfce5f5379f3a6b07282cbc7b`) into the final stage: a build stage runs `nix copy --no-check-sigs --to /out <nix>`, which writes a registered store, and the final stage copies `/out/nix` to `/nix` and links `/usr/local/bin/nix`. The Determinate installer was **not** run: it downloads whatever it currently serves (not reproducible from a tag), adds users and a daemon unit we would then disable, and the whole `nixos/nix` profile is 788 MB (bash, coreutils, git, curl, man-db, ...) where the closure of `nix` alone is 121 MB. The CA bundle is Debian's (`NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt`). `nix.conf` is the seven settings of the design plus `connect-timeout = 20` and an empty `build-users-group` (without it every command warns that group `nixbld` doesn't exist).

All of the following ran in `docker run --platform linux/amd64` on the built image (Apple silicon, so emulated), as root, with `nixpkgs` pinned to `ac62194c3917d5f474c1a844b6fd6da2db95077d` (head of `nixos-25.05` on 2026-10-17); `<rev>` below is that commit and `<enc>` is `sha256-16KkgfdYqjaeRGBaYsNrhPRRENs0qzkQVUooNHtoy2w%3D`.

| Check | Command, result |
|---|---|
| Build as root, no daemon | `nix build --max-jobs 0 --no-link --print-out-paths github:NixOS/nixpkgs/<rev>#hello` printed `/nix/store/na5dnb0l6rjkzrbld3xn4188dd8nknhh-hello-2.12.1` (5 paths, 9.7 MiB; 11 s, emulated, unreliable) |
| Runs as `nobody` | `setpriv --reuid=65534 --regid=65534 --clear-groups --no-new-privs -- /nix/store/na5dnb0l6rjkzrbld3xn4188dd8nknhh-hello-2.12.1/bin/hello` printed `Hello, world!`, exit 0 |
| Uncached derivation refused | `nix build --max-jobs 0 --no-link --print-out-paths --impure --expr '(builtins.getFlake "github:NixOS/nixpkgs/<rev>").legacyPackages.x86_64-linux.hello.overrideAttrs (o: { name = "hello-uncached-xyz"; })'`: `error: Cannot build '/nix/store/i54spgdvyb166vgw6cn7582vd0m1fj84-hello-uncached-xyz.drv'. Reason: local builds are disabled (max-jobs = 0)`, exit 1 |
| Wrong `narHash` refused | `nix build --max-jobs 0 --no-link --print-out-paths "github:NixOS/nixpkgs/<rev>?narHash=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA%3D#hello"`: `error: NAR hash mismatch in input '...', expected 'sha256-16KkgfdYqjaeRGBaYsNrhPRRENs0qzkQVUooNHtoy2w=' but got 'sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='`, exit 102 |
| Hash from `nix flake metadata` | `nix flake metadata --json github:NixOS/nixpkgs/<rev>` gives `narHash` `sha256-16KkgfdYqjaeRGBaYsNrhPRRENs0qzkQVUooNHtoy2w=` |
| Percent-encoded correct hash accepted (the form the core builds) | `nix build --max-jobs 0 --no-link --print-out-paths "github:NixOS/nixpkgs/<rev>?narHash=<enc>#hello"` printed the same `hello` path, exit 0 |
| Same, one character changed, refused | `...?narHash=sha256-A6KkgfdYqjaeRGBaYsNrhPRRENs0qzkQVUooNHtoy2w%3D#hello`: `error: NAR hash mismatch in input '...', expected 'sha256-16Kk...y2w=' but got 'sha256-A6KkgfdYqjaeRGBaYsNrhPRRENs0qzkQVUooNHtoy2w='`, exit 102 |
| The Rust set | `nix build --max-jobs 0 --no-link --print-out-paths <rev>#rustc <rev>#cargo <rev>#pkg-config <rev>#openssl.dev`: 24 paths fetched, 298.7 MiB download, 1.4 GiB unpacked, exit 0, 54 s under emulation on a laptop connection (unreliable) |

Closures for `x86_64-linux` at the same commit, from narinfo queries on `cache.nixos.org` (download is the sum of compressed NAR sizes, unpacked the sum of NAR sizes; the attribute's `outputsToInstall` outputs, which is what `nix build` fetches):

| Packages | Paths | Download | Unpacked |
|---|---|---|---|
| `cargo rustc` (1.86.0) | 26 | 311 MiB | 1.47 GiB |
| `go` (1.24.10) | 9 | 56.6 MiB | 278 MiB |
| `nodejs` (22.20.0) | 27 | 50.0 MiB | 204 MiB |
| `python3` (3.12.12) | 23 | 64.4 MiB | 171 MiB |
| `rustc cargo pkg-config openssl.dev` | 33 | 312 MiB | 1.48 GiB |

(The build above fetched 24 paths because some of the 33 are already in the image's own store: glibc, zlib and the like.)

Memory: evaluating `github:NixOS/nixpkgs/<rev>#cargo.drvPath` in a fresh container peaked at 846 MiB (`memory.peak` of the container's cgroup; it includes page cache, so it overstates the process). A whole container that evaluated and fetched the Rust set peaked at 2.24 GiB of cgroup memory, mostly page cache from writing 1.4 GiB of store; that figure is an upper bound, not what Nix needs. A `standard-1` instance (4 GiB memory, 8 GB disk) has room for the Rust set (2.0 GiB of store plus the 3 GB image is the disk question, see DEPLOY.md).

Image: unpacked 2.92 GB before, 3.05 GB after (+132 MB: 127 MB for `/nix`, 4.8 MB for the `nix` binary). Compressed (`docker save | gzip -1`) 751 MB after; the added layers compress to 47 MB at `gzip -1` (42.5 MB at `-6`), so about 704 MB before, which matches the 710 MB DEPLOY.md gave; "before" is derived by subtraction, not from a separate build.

## Not verified

- Nothing ran inside a Cloudflare Container. The `x86_64` runs were under emulation on a laptop, so their timings mean little, and no run was on native `x86_64`.
- Memory of Nix evaluation on a Cloudflare container: the figures above are cgroup peaks including page cache, measured on a laptop's Docker VM, not on Cloudflare. Disk: 3.05 GB image plus up to 2 GiB of store for a Rust environment and a project's build, against the runner's `standard-2` (12 GB), is arithmetic, not a run (DEPLOY.md).
- The Determinate installer was not tried; the copied-closure install was chosen on reproducibility and size. Whether a newer `nixos/nix` tag keeps `nix copy --to` working this way is untested.
- The prover's realisation step has run only in the image under `linux/amd64` emulation, as root, for `hello` and `openssl`. Not tried: `collective run`/`prove` on an operator's machine with a multi-user (daemon) Nix install, a realisation behind `PROVER_SETUP_PASSTHROUGH`'s proxy, or a timeout of the 600-second limit.
- Substitution settings on an operator's machine. The image's `nix.conf` pins `cache.nixos.org`, its key and `require-sigs`; elsewhere the prover passes only `--max-jobs 0` and `allow-import-from-derivation false`, and which substituters and keys are trusted is the operator's own `nix.conf` (Nix's defaults are `cache.nixos.org` with signatures required). `collective doctor` reports whether builds are disabled there, not the substituters.
- Warm-store behaviour (a second realisation in the same container) and concurrent realisations were not measured.
- How far prover-composed environments go for real projects: the first measurements should be resvg (pure Rust), a Rust project with `openssl-sys`, and a Python project with native extensions.
- A proposal's proof against a real prover end to end. The prover now runs an empty diff as "patched = base" (tested), but no proposal has been settled by real runners; the e2e test uses synthetic reports.
