# CLAUDE.md — `meridian-core`

Framework-free core workspace for [Meridian](https://github.com/Yuerchu/meridian).
Everything here runs without Tauri, without a window, and without a WebView.

## Crates

| Crate | Purpose |
|---|---|
| `core` (`meridian-core`) | Agent turn loop, providers, tools, MCP, sandboxed execution, SeaORM persistence, secrets, OneBot, hooks, offline voice, balance/spending watcher |
| `meridiand` | Headless balance/spending watcher daemon |
| `litert-lm` | C ABI wrapper for LiteRT-LM local inference |
| `sandbox-types` / `sandbox-windows` | Sandboxed process execution primitives |

## Hard Engineering Standards

### 1. Crate & Runtime Boundaries
- **No UI framework dependencies.** `meridian-core` never depends on Tauri or any windowing crate.
- **No `block_on` in core production code.** Sync-to-async bridging happens only once in the desktop shell setup and once in `meridiand`'s `main`.
- **No `[workspace.package]` inheritance.** The desktop repo consumes these crates by path from an outer workspace that `exclude`s this directory.

### 2. Wire Casing & Strict Contract Serialization
- **First-party wire / event / request DTOs use Rust-first `snake_case`.**
  Do NOT put `#[serde(rename_all = "camelCase")]` on first-party request or event types (e.g., `WorkspaceReferenceRequest`, `ChatStreamEvent`).
- **Third-party external protocol adapters are the only exception.**
  Types modeling external APIs whose spec mandates `camelCase` (e.g., Google Gemini API in `provider/google_*.rs` or external JSON-RPC specs) match the external spec and must stay isolated inside their adapter module.
- **Closed objects, no compatibility aliases.**
  Every first-party `Deserialize` struct must carry `#[serde(deny_unknown_fields)]`. Never add `#[serde(alias = ...)]`, `#[serde(other)]`, `#[serde(flatten)]`, or `#[serde(default)]` to silently paper over schema skew.
- **Explicit nullable keys.**
  Nullable keys on first-party wire inputs/events use `deserialize_required_nullable` (key must be present; absence is an error, `null` means unset). Never downgrade malformed first-party JSON into empty/default values.
- **No invented domain defaults.**
  Missing model/provider/config limits remain `None` rather than inventing fallback numbers.

### 3. Persistence & Type Safety (`db/`)
- **Persistence entities never cross IPC or public protocol boundaries.**
  SeaORM `entity::*::Model` / `ActiveModel` live strictly under `core/src/db/entity/` and never derive `Serialize`. Do not create `type Foo = entity::Model` aliases or bare-import `Model` / `ActiveModel` / `Entity` / `Column` outside `db/entity/`.
- **Exact money, booleans, and timestamps.**
  - Money uses `crate::decimal::Decimal` (`NUMERIC(38,18)`, canonical string in JSON and SQLite `TEXT`). Never use `f32`/`f64` or SeaORM's `with-bigdecimal` feature.
  - SQLite integer flags use `SqlBool` (strict `0`/`1` decoding).
  - Timestamps use `EpochMs`.
  - JSON columns are explicitly decoded at boundaries — never use `FromJsonQueryResult`.
- **Explicit transaction boundaries & SQL neutrality.**
  - Native SQL is confined to `db/sql.rs` and migration/bridge/baseline files.
  - `Expr::cust` outside migrations requires a `backend: sqlite-only` comment and must be registered in the desktop repo's `docs/backend-neutrality.md`.

### 4. `db/sea/ops/` Naming & API Conventions
Every table owns one module under `core/src/db/sea/ops/<resource>.rs`. Callers invoke ops qualified by module (`assistant::list`, `conversation::create`, `provider::update`), so functions follow idiomatic Rust module-scoped naming:

- **Do not repeat the module's singular/plural noun in function names** when the module manages a single primary resource:
  - Prefer `list`, `list_enabled`, `list_by_project`, `list_for_assistant` (not `list_assistants`, `list_providers`, `list_mcp_servers`).
  - Prefer `get`, `get_default`, `get_by_key`, `get_by_name`, `find_by_source`, `find_by_path` (not `get_assistant`, `get_project`, `find_project_by_path`).
  - Prefer `create`, `insert`, `upsert`, `update`, `delete`, `soft_delete`, `restore`, `purge`, `count`, `validate` (not `create_provider`, `update_memory`, `delete_skill`).
  - Multi-entity modules (`notification` managing webhooks + alert state; `plan_review` managing documents/revisions/reviews/deliveries; `todo` managing lists + items) keep the sub-entity noun to disambiguate (`list_webhooks`, `get_review`, `list_items`).
- **Prefer idempotent state setters over UI toggle semantics.**
  Write `set_pinned(tx, id, pinned: bool, now)` / `set_archived(tx, id, archived: bool, now)` instead of `toggle_pin` / `toggle_archive`, so higher-level atomic patch commands do not have to read-compare-flip.
- **Canonical domain vocabulary.**
  Use `emoji` consistently across ops, errors, and helpers — do not introduce `sticker` aliases (existing SQLite table/column names like `message_stickers.sticker_id` stay aligned with the schema).

## Verification Gates

Before pushing any change to `meridian-core`:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

(On Windows standalone checkouts without sibling `src-tauri/resources/`, prepare `LITERT_LM_LIB_PATH` via `./.github/scripts/prepare-litert-runtime.ps1`.)
