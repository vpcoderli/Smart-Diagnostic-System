# Copilot instructions for Smart Diagnostic System

## Build, test, and lint commands

Run workspace commands from the repository root unless noted.

```sh
# Compile only, no GUI bundling
cargo build
cargo build -p diag-core
cargo build -p smart-diag-collector
cargo build -p smart-diag-analyzer

# Tauri dev/build commands must run inside each app's src-tauri directory
cd collector/src-tauri && cargo tauri dev
cd analyzer/src-tauri && cargo tauri dev
cd collector/src-tauri && cargo tauri build
cd analyzer/src-tauri && cargo tauri build

# Tests
cargo test
cargo test -p diag-core
cargo test -p smart-diag-analyzer
cargo test -p diag-core test_package_build_and_read_roundtrip
cargo test -p diag-core url_resolver::tests::test_resolve_pcm_management_url
cargo test -p smart-diag-analyzer test_analyze_requests_risk_levels

# Rust formatting/linting
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

The frontends are plain HTML/CSS/JavaScript with no npm build step. Both Tauri configs point `frontendDist` directly at `../src/`, and Tauri's global API is enabled with `withGlobalTauri`.

## High-level architecture

This repository is a Rust workspace for a two-app diagnostic workflow:

- `crates/diag-core` is the shared library for models, URL/log/SQL parsing, privacy masking, package read/write code, and the `LogCollector` / `ServiceDiscovery` traits.
- `collector/src-tauri` is the hospital-intranet collection app. It imports deployment CSV/ELK configuration, validates SSH/DB/ELK connectivity, captures browser traffic through a Tauri WebView, collects logs through ELK or SSH, extracts SQL, runs DB/EXPLAIN collection, and writes diagnosis ZIP output.
- `analyzer/src-tauri` is the developer-side analysis app. It imports a diagnosis ZIP through `diag_core::package::read_package()`, summarizes requests/logs/SQL, runs `rule_engine::diagnose()`, and generates a Markdown report.

Collector state lives in `commands::AppState` and is managed by Tauri. The UI enforces a phased flow: configure/import, validate connections, then collect. Keep the Rust command state, the frontend `state` object in `collector/src/app.js`, and the Tauri invoke handler list in `collector/src-tauri/src/lib.rs` synchronized when adding collector capabilities.

Collector collection modes should converge on the same diagnosis pipeline once trace IDs are known:

- realtime: diagnostic browser captures fetch/XHR traffic, then logs are queried by captured trace IDs;
- history: user-provided trace IDs or keywords drive ELK lookup without browser capture;
- scheduler: background ELK checks for ERROR/WARN, trace IDs are deduplicated, diagnosis runs periodically, and `scheduler-package-created` events notify the frontend;
- quick: log/SQL/EXPLAIN/table-stat output is packaged through the quick TXT/Markdown ZIP path.

Log collection is intentionally pluggable through `diag_core::collector_trait::LogCollector`. `elk_collector.rs` auto-detects direct Elasticsearch vs Kibana proxy mode and uses configurable field mappings; `ssh_log_collector.rs` is the SSH-grep fallback path.

Package compatibility matters across apps. `diag-core::package` contains the structured JSON package reader/writer used by the analyzer and quick TXT/Markdown ZIP output used by collector quick/diagnosis paths. When changing package contents, update producer and consumer expectations together.

## Key conventions

- User-facing UI text and Tauri command messages are Chinese. Comments are mixed Chinese/English.
- Shared JSON/Tauri DTOs use `#[serde(rename_all = "camelCase")]`; frontend code expects camelCase fields.
- Frontends use global Tauri APIs (`window.__TAURI__.core.invoke`) from plain JavaScript. There is no framework, bundler, package manager workflow, or generated frontend build artifact.
- The diagnostic WebView sends captured data back to Rust through the `diag://` custom scheme: `diag://collect` carries captured requests and `diag://count` updates live request counts.
- URL-to-service parsing should go through `diag_core::url_resolver` and the configured gateway prefix. Do not hardcode `/gateway` in analyzer/collector logic when `manifest.gateway_prefix` or `config.gateway.prefix` is available.
- Known PCM service names live in `crates/diag-core/src/url_resolver.rs`; add new `pcm-*` services there when deployment coverage grows.
- Privacy masking defaults to masking query parameter values. Only configured allowlist keys such as `pageNum`, `pageSize`, and `portal` keep values; sensitive headers such as `authorization`, `cookie`, `set-cookie`, and token headers are filtered.
- Collector config is derived from deployment manifests in `deployment::manifest_to_collector_config()` and persisted via `config_store` under the Tauri app data directory as `deployment-config.json`.
- ELK and HTTP clients accept self-signed certificates (`danger_accept_invalid_certs`) because hospital intranets commonly use internal TLS.
- SSH host key verification is disabled in the current MVP-style collector flow; treat that as an explicit intranet assumption, not a general security pattern.
- Analyzer risk thresholds are code-defined: request ERROR is HTTP 500+, SLOW is over 2000ms, WARN is 4xx or over 1000ms; SQL risk rules consider duration, scan amplification, and index usage.
