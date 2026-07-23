# Pet Health Agent Guide

## Stack

- Rust + Axum
- Askama server-rendered HTML
- HTMX bounded fragment updates
- SeaORM over SQLite in WAL mode
- Rig for optional model-backed event extraction

## Product invariants

- Every pet, event, share, and audit query must be household-scoped.
- The LLM never receives database write access and never supplies authoritative timestamps.
- Preserve the user's original health-event wording alongside structured fields.
- Vet access must be revocable, expiring, least-privilege, and non-enumerable.
- `PRODUCTION=true` always stores SQLite data at `/persistent/pethealth.sqlite`.
- Never commit real pet health records, database files, credentials, or share tokens.
- AI output and linked knowledge are context, not diagnosis.

## Verification

Run `cargo fmt --check`, `cargo check --all-targets`, and `cargo test`. For UI changes, test the actual HTMX capture and undo flow at desktop and mobile widths.

