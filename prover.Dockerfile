# Proof sandbox image for Cloudflare Containers (see docs/PROOFS.md).
# Build context: repository root.
#
# Toolchains are copied from official images rather than installed with a
# package manager, so the image is reproducible from pinned tags.

FROM rust:1.96-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p collective-prover

FROM golang:1.25-bookworm AS go
FROM node:22-bookworm-slim AS node

# Nix (docs/ENVIRONMENTS.md): only the closure of the `nix` binary is taken
# from the pinned official image (not its whole 788 MB profile), registered
# in a store of its own so it can be copied into the final stage as /nix.
FROM nixos/nix:2.35.3 AS nix
RUN mkdir -p /out/usr/local/bin \
 && NIX=$(readlink -f "$(command -v nix)") \
 && nix --extra-experimental-features nix-command copy --no-check-sigs --to /out "${NIX%/bin/nix}" \
 && ln -s "$NIX" /out/usr/local/bin/nix

# python:*-bookworm is buildpack-deps:bookworm: git, gcc/make, and
# util-linux (unshare, setsid, setpriv) are already present.
FROM python:3.13-bookworm
COPY --from=go /usr/local/go /usr/local/go
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx
COPY --from=build /usr/local/rustup /usr/local/rustup
COPY --from=build /usr/local/cargo /usr/local/cargo
# Nix, single-user, as root, no daemon. Environments are substituted from
# cache.nixos.org and never built (max-jobs = 0), so there is no nixbld group
# and no Nix sandbox. /nix/store is root-owned and world-readable.
COPY --from=nix /out/nix /nix
COPY --from=nix /out/usr/local/bin/nix /usr/local/bin/nix
RUN mkdir -p /etc/nix && printf '%s\n' \
    'experimental-features = nix-command flakes' \
    'substituters = https://cache.nixos.org' \
    'trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=' \
    'require-sigs = true' \
    'max-jobs = 0' \
    'allow-import-from-derivation = false' \
    'sandbox = false' \
    'connect-timeout = 20' \
    'build-users-group =' > /etc/nix/nix.conf
ENV NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
ENV RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:/usr/local/go/bin:$PATH
COPY --from=build /src/target/release/collective-prover /usr/local/bin/collective-prover

# Untrusted code runs here: no secrets are ever put in this image or its
# environment, and each run drops to an unprivileged user.
WORKDIR /tmp
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/collective-prover", "serve", "8080"]
