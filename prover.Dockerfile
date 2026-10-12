# Proof sandbox image for Cloudflare Containers (see docs/PROOFS.md).
# Build context: repository root.
#
# Toolchains are copied from official images rather than installed with a
# package manager, so the image is reproducible from pinned tags.

FROM rust:1.90-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p collective-prover

FROM golang:1.25-bookworm AS go
FROM node:22-bookworm-slim AS node

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
ENV RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:/usr/local/go/bin:$PATH
COPY --from=build /src/target/release/collective-prover /usr/local/bin/collective-prover

# Untrusted code runs here: no secrets are ever put in this image or its
# environment, and each run drops to an unprivileged user.
WORKDIR /tmp
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/collective-prover", "serve", "8080"]
