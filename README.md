# Pet Health

Pet Health is an AGPL-3.0 self-hostable health timeline for multi-pet households. It uses Rust, Axum, Askama, HTMX, SeaORM, SQLite/WAL, and Rig for optional LLM-backed structured capture.

The first complete workflow is intentionally direct:

> An owner can write “Milo vomited just now”, immediately see a timestamped structured event on Milo's timeline, and undo it.

The application also creates expiring, revocable, read-only vet links. Share tokens are stored only as SHA-256 hashes and are displayed to the owner once.

## MCP for Codex and Claude

Pet Health exposes an authenticated MCP endpoint at `/mcp`. It uses the same expiring, revocable account sessions as the web app: send the session token as `Authorization: Bearer <session-token>` or use the browser session cookie. Every tool call is scoped to that account's household.

The available tools cover listing pets, reading a pet timeline, reading care context, adding a pet, recording an observation, and undoing an event. Write tools are explicit, preserve the user's original wording, and let the server choose timestamps.

For Claude Code, add the endpoint with a bearer token from an active Pet Health session:

```bash
claude mcp add --transport http pethealth https://your-host/mcp \\
  --header "Authorization: Bearer $PETHEALTH_SESSION_TOKEN"
```

For Codex, add the same URL and bearer token as a remote MCP server in the MCP settings for your Codex client. Keep write actions enabled only for users who should be able to change the household record. OAuth discovery and a consent screen are the next auth layer; the current bearer token is already revocable from Account settings because it is an ordinary app session.

## Run locally

```bash
cp .env.example .env
set -a
source .env
set +a
cargo run
```

Open `http://localhost:3000`. On a new database, `APP_USERNAME` and `APP_PASSWORD` create the initial owner account for the default household. They are bootstrap credentials only; after the account exists, password changes happen from **Account settings** and revoke every active session.

Additional owners can register with an email address. Each registration creates a separate household, and all pet, event, share, and audit queries remain household-scoped. Passwords use salted Argon2 hashes. Browser sessions are random, stored server-side as SHA-256 token hashes, expire after 30 days, and use `HttpOnly`, `SameSite=Lax`, and production-only `Secure` cookies.

## Production data

When `PRODUCTION=true`, the database location is not configurable: it resolves to:

```text
/persistent/pethealth.sqlite
```

The process refuses to start if `/persistent` is absent or the production password is unchanged. Mount durable local storage at `/persistent`; SQLite runs in WAL mode with foreign keys, a busy timeout, and `synchronous=FULL`.

The checked-in `compose.production.yml` is the Coolify production definition. It deliberately has no host port mapping, pulls the public `ghcr.io/dunctk/pethealth:mvp` image, and declares the durable volume explicitly:

```bash
docker build -t pethealth:mvp .
APP_USERNAME=owner APP_PASSWORD='replace-me' docker compose -f compose.production.yml up -d
```

In Coolify, route the `pethealth` service to `https://your-host:3000`. Keep the bootstrap `APP_PASSWORD` in Coolify's protected environment settings rather than adding it to the compose file. Back up the named volume containing `/persistent/pethealth.sqlite` together with its WAL files.

## GitHub Actions image publishing

The `main` branch workflow runs the Rust checks and publishes `ghcr.io/dunctk/pethealth:mvp` to GitHub Container Registry using the built-in `GITHUB_TOKEN`. No repository secrets are needed.

After the first successful publish, open the package settings on GitHub and set the `pethealth` container package visibility to **Public**. Coolify pulls the image without credentials.

## Optional Rig agent

Common health phrases are handled locally so capture remains available without a provider. Configure the following to let Rig structure observations outside the deterministic vocabulary:

```text
LLM_API_KEY=...
LLM_BASE_URL=https://openrouter.ai/api/v1
LLM_MODEL=openai/gpt-4.1-mini
```

The model proposes a typed event only. Rust resolves the pet, validates the proposal, chooses the timestamp, and performs the tenant-scoped transaction.

## Verification

```bash
cargo fmt --check
cargo check --all-targets
cargo test
docker build -t pethealth:mvp .
```
