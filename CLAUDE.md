# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Smart Diagnostic System is a dual-application diagnostic pipeline for toB medical projects (PCM — Patient Care Management). It solves the problem of diagnosing issues in hospital intranets where developers have no direct access.

Two desktop apps built with Rust + Tauri 2.x:
- **Collector** (`smart-diag-collector`): Runs on a Windows jump server inside the hospital intranet. Captures HTTP traffic via JS injection, queries logs (SSH or ELK), queries DB for slow SQL, and packages everything into `diagnosis.zip`.
- **Analyzer** (`smart-diag-analyzer`): Runs on the developer's machine. Imports `diagnosis.zip`, runs a rule engine, and exports a Markdown report.

## Build & Run

**Prerequisites:** Rust stable toolchain, Tauri CLI (`cargo install tauri-cli`), Node.js (required by Tauri bundler even though frontends are plain HTML/JS/CSS).

```sh
# Dev mode — must run from within the app's src-tauri directory
cd collector/src-tauri && cargo tauri dev
cd analyzer/src-tauri && cargo tauri dev

# Release build
cd collector/src-tauri && cargo tauri build
cd analyzer/src-tauri && cargo tauri build

# Compile only (no GUI, from workspace root)
cargo build
cargo build -p diag-core
cargo build -p smart-diag-collector

# Tests
cargo test                    # all workspace
cargo test -p diag-core       # shared lib only
```

Frontends have no build step — `frontendDist` in `tauri.conf.json` points directly to `../src/`.

## Architecture

```
[Hospital Intranet]                          [Developer Machine]
┌──────────────────────────────┐             ┌────────────────────────────────┐
│   smart-diag-collector       │             │   smart-diag-analyzer          │
│                              │             │                                │
│  Phase 1: Import CSV + ELK   │             │  1. Import diagnosis.zip       │
│  Phase 2: Validate conns     │  diagnosis  │  2. run_analysis()             │
│  Phase 3: Collect (3 modes)  │  .zip ────► │  3. rule_engine::diagnose()    │
│    realtime / history / sched │             │  4. Export Markdown report     │
└──────────────────────────────┘             └────────────────────────────────┘
```

### Workspace Members

- `crates/diag-core` — shared library: models, parsers, masking, ZIP packaging, traits (`LogCollector`, `ServiceDiscovery`)
- `collector/src-tauri` — Collector app backend
- `analyzer/src-tauri` — Analyzer app backend

### Collector Modules

| Module | Role |
|--------|------|
| `commands.rs` | Tauri command handlers; holds `AppState` |
| `deployment.rs` | CSV parsing, `DeploymentManifest`, template generation |
| `validator.rs` | SSH + DB connectivity validation |
| `webview_capture.rs` | Opens diagnostic browser with JS injection for HTTP interception |
| `diagnosis.rs` | `DiagnosisRunner` — orchestrates real-time and historical collection |
| `elk_collector.rs` | `LogCollector` impl via Elasticsearch (direct or Kibana proxy, auto-detected) |
| `ssh_log_collector.rs` | `LogCollector` impl via SSH `grep` on remote servers |
| `nacos_discovery.rs` | `ServiceDiscovery` impl querying Nacos registry |
| `sql_extractor.rs` | Extracts SQL from log lines (MyBatis/Hibernate/generic patterns) |
| `explain_collector.rs` | Runs `EXPLAIN` on slow SQLs (MySQL + PostgreSQL) |
| `db_collector.rs` | Queries slow SQL stats from database |
| `scheduler.rs` | Background scheduled diagnosis runs (cron-like), emits Tauri events |
| `dedup_cache.rs` | traceId dedup with time-windowed expiry |
| `config_store.rs` | Persists `DeploymentManifest` to `app_data_dir/deployment-config.json` |
| `cleanup.rs` | Removes old `.zip` packages past retention window |

### Three Collection Modes (Collector)

1. **Realtime** — user opens diag browser, reproduces issue, JS captures HTTP traffic, then `DiagnosisRunner` collects logs by traceId
2. **Historical** — user provides traceId list + time window; `DiagnosisRunner` queries ELK directly without browser capture
3. **Scheduler** — periodic background runs: queries ELK for error keywords, deduplicates traceIds, runs diagnosis, emits `scheduler-package-created` Tauri event

### Log Collection Strategy

The collector supports two pluggable `LogCollector` backends:
- **ELK** (`elk_collector.rs`): auto-detects direct ES vs. Kibana proxy mode; supports ES 6.x/7.x/8.x query syntax differences
- **SSH** (`ssh_log_collector.rs`): SSHes into app servers, `grep`s log files by traceId

Frontend `state.logSource` = `'elk' | 'ssh'` selects which backend to use.

### Key Data Flow

1. User imports CSV deployment docs → `deployment::manifest_to_collector_config()` builds `CollectorConfig`
2. Config auto-saved to `app_data_dir/deployment-config.json` and reloaded on next launch
3. Validation phase tests SSH and/or ELK connectivity, pings DB with `sqlx`
4. `webview_capture.rs` opens a second `WebviewWindow` with injected JS that intercepts all `fetch`/XHR calls and POSTs them back via `diag://collect`; request count via `diag://count`
5. `DiagnosisRunner` orchestrates: group requests by service → query logs (ELK or SSH) per traceId → extract SQL from logs → run EXPLAIN on slow SQL → query DB stats → privacy masking → `diag_core::package::build_package()` → ZIP
6. ZIP structure: `manifest.json`, `browser/requests.json`, `services/{svc}/app-log.jsonl`, `database/slow-sql.json`, `database/explain-plans.json`, `database/table-stats.json`, `privacy/masking-report.json`

### State Management (Collector)

`AppState` held in Tauri managed state:
- `manifest: Mutex<Option<DeploymentManifest>>` — parsed CSV data
- `config: Mutex<Option<CollectorConfig>>` — derived runtime config
- `validated: Mutex<bool>` — gate for phase 3
- `scheduler_status: Arc<Mutex<SchedulerStatus>>` — shared with background task
- `scheduler_handle: Mutex<Option<SchedulerHandle>>` — shutdown channel

The three-phase workflow (import → validate → collect) is enforced by checking this state in command handlers.

## Key Conventions

- **All user-facing strings are in Chinese.** Code comments are mixed Chinese/English.
- **All shared `serde` types use `camelCase` JSON** (see `models.rs`).
- **Privacy masking:** `mask_query_values = true` by default. Allowed query params: `pageNum`, `pageSize`, `portal`, `type`, `status`. Everything else becomes `***`.
- **`diag://` custom scheme** is the data channel from the diagnostic WebView to Rust. `diag://collect` for request data, `diag://count` for real-time counts.
- **Output path:** `./diagnosis-output/YYYYMMDD-HHMMSS-<page-path-slug>.zip`
- **`Cargo.lock` is gitignored** — unusual for binary apps, but that's the current setup.
- **`url_resolver.rs`** has a hardcoded list of known `pcm-*` service names. Add new services there when the deployment grows.
- **SSH host key verification is disabled** (accepts all keys) — MVP limitation for hospital intranet use.
- **Self-signed TLS certs accepted** in ELK and HTTP clients (`danger_accept_invalid_certs`) — common in hospital intranets.
- **ELK field mapping is configurable** via `FieldMapping` struct; defaults match standard Logstash output.
- **Analyzer risk thresholds:** HTTP ERROR ≥ 500, SLOW > 2000ms, WARN > 1000ms or 4xx; SQL SLOW > 1000ms avg, scan amplification > 100x.

## Frontend

Single-page plain HTML/JS/CSS per app (no framework, no build step). The collector frontend at `collector/src/app.js` manages a 3-phase wizard UI and communicates with Rust via `window.__TAURI__.core.invoke`. Tauri events (`scheduler-package-created`) push notifications from background tasks.

## Design Document

Full MVP design rationale is at `docs/plans/2026-05-08-smart-diagnostic-mvp-design.md`.
