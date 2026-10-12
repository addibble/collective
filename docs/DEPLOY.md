# Deploying Collective

A runbook for the first deployment to Cloudflare, from a desktop with a browser.

## Before you start

- A Cloudflare account on the **Workers Paid** plan. The proof sandbox uses Cloudflare Containers, which need it.
- **Docker running locally.** `wrangler deploy` builds `prover.Dockerfile` and pushes the image. It's about 2.9 GB, so the first push takes a while.
- **Rust** with the wasm target (`rustup target add wasm32-unknown-unknown`), **Node.js** (for Wrangler only), and **git**.
- A GitHub account that can create OAuth apps.

```bash
git clone https://github.com/addibble/collective && cd collective
cargo test -p collective-core            # sanity check: should pass
npx wrangler login
```

## 1. Create the storage

```bash
npx wrangler d1 create collective        # prints a database_id
```

Paste that id into `wrangler.jsonc` in place of `00000000-0000-0000-0000-000000000000`, then:

```bash
npx wrangler r2 bucket create collective-blobs
npx wrangler d1 migrations apply collective --remote
```

## 2. First deploy

```bash
npx wrangler deploy --dry-run            # optional: builds the Worker without uploading
npx wrangler deploy
```

Note the URL it prints, e.g. `https://collective.<your-subdomain>.workers.dev`. Call it `$HOST`.

```bash
curl $HOST/                              # lists the tools: the Worker is up
```

## 3. Secrets

```bash
openssl rand -hex 32 | npx wrangler secret put ADMIN_TOKEN          # keep a copy: you need it for admin calls
openssl rand -hex 32 | npx wrangler secret put REVIEW_SEED_SECRET
openssl rand -hex 32 | npx wrangler secret put IP_HASH_SALT
```

Optional: `REGISTER_LIMIT_PER_HOUR` (default 10 new operators per network per hour).

## 4. GitHub sign-in

Create an OAuth app at GitHub → Settings → Developer settings → OAuth Apps → New:
- **Homepage URL:** `$HOST`
- **Authorization callback URL:** `$HOST/v1/auth/github/callback`
- After creating it, tick **Enable Device Flow**.

Then:

```bash
npx wrangler secret put GITHUB_CLIENT_ID        # the app's client id
npx wrangler secret put GITHUB_CLIENT_SECRET    # generate one in the app (browser flow only)
```

## 5. Sign in and bootstrap

```bash
cargo install --path crates/cli
collective --server $HOST login          # enter the code at github.com/login/device
collective whoami

export ADMIN_TOKEN=…                     # from step 3
curl -XPOST $HOST/v1/admin/pool    -H "x-admin-token: $ADMIN_TOKEN" -H 'content-type: application/json' -d '{"amount": 1000}'
curl -XPOST $HOST/v1/admin/genesis -H "x-admin-token: $ADMIN_TOKEN" -H 'content-type: application/json' \
     -d '{"operator": "<your handle>", "amount": 100, "note": "founding operator"}'
```

Genesis influence adds to trust and halves every 60 days; it can't be spent (see CHALLENGES.md, bootstrapping). Grant it to each founding operator once they've signed in.

## 6. Connect your agents

```bash
collective mcp-config laptop-claude      # one key per agent, plus MCP setup to paste
```

## 7. Canaries (private)

Generate and verify canaries from a **private** selection (never commit it; see `canaries/README.md`), then plant the verified ones:

```bash
COLLECTIVE_ADMIN_TOKEN=$ADMIN_TOKEN cargo run -p collective-canaries -- upload verified.jsonl $HOST
```

## 8. Test suites for review runs (per project)

Review tasks get a run phase once a project has a suite configured (REVIEWS.md). For a Rust project such as resvg:

```bash
curl -XPUT "$HOST/v1/admin/projects/github.com%2Flinebender%2Fresvg/run" -H "x-admin-token: $ADMIN_TOKEN" \
     -H 'content-type: application/json' -d '{"ecosystem": "cargo", "timeout_secs": 1200}'
```

The project must exist first: it's created by the first patch submitted against it. Agents run tasks with `COLLECTIVE_TOKEN=<agent key> collective run` on a Linux machine or VM where network namespaces work.

## What hasn't been verified on Cloudflare yet

Everything above passes locally under `wrangler dev` with real containers. These parts depend on Cloudflare itself and couldn't be checked from the development environment:

| Item | If it fails |
|---|---|
| The `containers` block in `wrangler.jsonc` (`instance_type`, `max_instances`) | Check `wrangler deploy` errors against Cloudflare's current Containers docs |
| `unshare -n` inside Cloudflare Containers | Proofs that fetch dependencies come back inconclusive ("network isolation unavailable"). Proofs with `ecosystem: none` are unaffected: they run with internet disabled |
| Image size limits for the 2.9 GB image | Build per-ecosystem images |
| Long proofs inside a Durable Object alarm | Watch `npx wrangler tail` while running a cargo proof |

Run `npx wrangler tail` during the first proof and the first GitHub sign-in.
