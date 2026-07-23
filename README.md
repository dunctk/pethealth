# Pet Health

Pet Health is an AGPL-3.0 self-hostable health timeline for multi-pet households. It uses Rust, Axum, Askama, HTMX, SeaORM, SQLite/WAL, and Rig for optional LLM-backed structured capture.

The first complete workflow is intentionally direct:

> An owner can write “Milo vomited just now”, immediately see a timestamped structured event on Milo's timeline, and undo it.

The application also creates expiring, revocable, read-only vet links. Share tokens are stored only as SHA-256 hashes and are displayed to the owner once.

## Weights and blood tests

The console keeps a dated weight history for each pet. Blood-test files are stored under a private household directory beneath `BLOOD_TESTS_DIR` (local default: `./example_blood_tests`; production compose default: `/persistent/blood_tests`). The web console can upload a PDF or image directly, and Syncthing can place files in the matching household directory. The owner then chooses **Import new tests**, or calls the MCP `upload_blood_test` / `import_blood_tests` tools.

Imports use Mistral OCR 4 (`mistral-ocr-4-0`) with block and table extraction. The OCR text is stored alongside parsed test name, value, unit, reference range, flag, and test date. Spanish and English labels are accepted, and the original OCR text remains available for review when a row cannot be parsed. Set `MISTRAL_API_KEY` in the environment or local `.env`; never commit that file.

## MCP for Codex and Claude

Pet Health exposes an authenticated MCP endpoint at `/mcp`. MCP clients can use OAuth with S256 PKCE, discovered from the standard `/.well-known/oauth-protected-resource` and `/.well-known/oauth-authorization-server` endpoints. Existing Pet Health session tokens also remain supported as revocable bearer tokens. Every tool call is scoped to that account's household.

The available tools cover listing pets, reading a pet timeline, reading care context, adding a pet, recording an observation, and undoing an event. Write tools are explicit, preserve the user's original wording, and let the server choose timestamps.

For Claude Code, add the endpoint and let Claude complete the OAuth consent flow:

```bash
claude mcp add --transport http pethealth https://your-host/mcp
```

For Codex, add the same URL as a remote MCP server. The client can discover the OAuth endpoints and ask the user to approve access. Keep write actions enabled only for users who should be able to change the household record.

If the client is running over SSH without a display, use the device-code fallback: the client posts its PKCE challenge to `/oauth/device`, prints the returned `user_code`, and polls `/oauth/token` with the returned `device_code`. Open the returned `verification_uri` on any phone or computer, enter the code, sign in, and approve. The code expires after 10 minutes and can be used once.

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
