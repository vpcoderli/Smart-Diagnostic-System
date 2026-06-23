# Realtime Diagnostic Report Format Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build Markdown realtime diagnostic reports that let developers and operators inspect one captured request as one complete incident scene: request metadata, risk summary, key logs, related SQL, EXPLAIN plans, and evidence links.

**Architecture:** Keep the feature inside `diag-core::package`, because realtime Markdown files are generated during ZIP packaging and already receive captured requests, logs, SQL traces, EXPLAIN plans, and table stats. Add `realtime/overview.md` and `realtime/request-cards.md`, preserve existing service-level raw files, and keep `realtime/request-logs.md` as a backward-compatible alias of the new request-card content. Pass the configured gateway prefix from the collector runtime into `build_realtime_package` so service/API parsing does not hardcode `/gateway`.

**Tech Stack:** Rust, `diag-core`, ZIP packaging via `zip`, Markdown string rendering, existing `diag_core::url_resolver::resolve_url`, integration tests in `crates/diag-core/tests/integration.rs`, collector runtime call site in `collector/src-tauri/src/diagnosis.rs`.

---

## File Structure

- Modify `crates/diag-core/src/package.rs`
  - Owns all Markdown rendering for realtime ZIP files.
  - Add private helpers for request indexing, risk classification, log prioritization, SQL/EXPLAIN/table rendering, and evidence links.
  - Change `build_realtime_package` to accept `gateway_prefix: &str`.
- Modify `collector/src-tauri/src/diagnosis.rs`
  - Pass `self.config.gateway.prefix.as_str()` into `build_realtime_package`.
- Modify `crates/diag-core/tests/integration.rs`
  - Add failing tests for `realtime/overview.md`, `realtime/request-cards.md`, missing trace handling, unmatched logs, and SQL/EXPLAIN/table co-location.
  - Update existing realtime package test to pass the gateway prefix argument.

No new production file is needed. `package.rs` is already the package renderer; splitting now would add module churn without improving the current feature boundary.

---

### Task 1: Add realtime overview index

**Files:**
- Modify: `crates/diag-core/tests/integration.rs`
- Modify: `crates/diag-core/src/package.rs`
- Modify: `collector/src-tauri/src/diagnosis.rs`

- [ ] **Step 1: Write the failing overview test**

Add this test after `test_realtime_package_groups_logs_by_captured_request_trace_id` in `crates/diag-core/tests/integration.rs`:

```rust
#[test]
fn test_realtime_package_outputs_overview_index() {
    use std::io::Read;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("realtime-overview-test.zip");
    let (pkg, _) = mock_diagnosis_package();

    build_realtime_package(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/gateway",
        &zip_path,
    )
    .unwrap();

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut overview = String::new();
    archive
        .by_name("realtime/overview.md")
        .expect("缺少实时诊断总览文件")
        .read_to_string(&mut overview)
        .unwrap();

    assert!(overview.contains("# 实时诊断报告"));
    assert!(overview.contains("| # | 风险 | traceId | 接口 | 状态码 | 耗时 | 服务 | 日志信号 | SQL | EXPLAIN |"));
    assert!(overview.contains("| 1 | ERROR | `trace-abc123` | /v1/patient/list | 200 | 3500ms | pcm-management | ERROR=1 WARN=0 | 1 | 0 成功 / 0 失败 |"));
    assert!(overview.contains("| 2 | OK | `trace-def456` | /v1/user/info | 200 | 150ms | pcm-user | ERROR=0 WARN=0 | 0 | 0 成功 / 0 失败 |"));
    assert!(overview.contains("| 3 | ERROR | `无 traceId` | /v1/patient/update | 500 | 200ms | pcm-management | ERROR=0 WARN=0 | 0 | 0 成功 / 0 失败 |"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
cargo test -p diag-core test_realtime_package_outputs_overview_index
```

Expected: compile failure because `build_realtime_package` does not accept `gateway_prefix`, or runtime failure because `realtime/overview.md` does not exist.

- [ ] **Step 3: Update the package API and call sites**

In `crates/diag-core/src/package.rs`, add `url_resolver` to the imports:

```rust
use crate::models::{
    CapturedPage, CapturedRequest, DiagnosisManifest, DiagnosisPackage, ExplainPlan, LogEntry,
    MaskingReport, SqlTrace, TableStats,
};
use crate::url_resolver;
```

Change the `build_realtime_package` signature and write the overview file before request-card files:

```rust
pub fn build_realtime_package(
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    explain_plans: &[ExplainPlan],
    table_stats: &[TableStats],
    captured_page: &CapturedPage,
    gateway_prefix: &str,
    output_path: &Path,
) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = std::fs::File::create(output_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    write_quick_contents(
        &mut zip,
        options,
        logs,
        sql_traces,
        explain_plans,
        table_stats,
    )?;

    zip.start_file("realtime/overview.md", options)?;
    zip.write_all(
        render_realtime_overview_md(captured_page, logs, sql_traces, explain_plans, gateway_prefix)
            .as_bytes(),
    )?;

    zip.start_file("realtime/request-logs.md", options)?;
    zip.write_all(render_realtime_request_logs_md(captured_page, logs).as_bytes())?;

    zip.finish()?;
    Ok(())
}
```

Update every test call in `crates/diag-core/tests/integration.rs` to pass `"/gateway"` before `&zip_path`:

```rust
build_realtime_package(
    &pkg.logs,
    &pkg.sql_traces,
    &pkg.explain_plans,
    &pkg.table_stats,
    &pkg.captured_page,
    "/gateway",
    &zip_path,
)
.unwrap();
```

Update `collector/src-tauri/src/diagnosis.rs`:

```rust
diag_core::package::build_realtime_package(
    &all_logs,
    &sql_traces,
    &explain_plans,
    &table_stats,
    captured,
    self.config.gateway.prefix.as_str(),
    &output_path,
)?;
```

- [ ] **Step 4: Implement minimal overview rendering**

Add these private helpers in `crates/diag-core/src/package.rs` before `render_realtime_request_logs_md`:

```rust
fn render_realtime_overview_md(
    captured_page: &CapturedPage,
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    explain_plans: &[ExplainPlan],
    gateway_prefix: &str,
) -> String {
    let mut md = String::new();
    md.push_str("# 实时诊断报告\n\n");
    md.push_str(&format!("页面 URL：{}\n\n", captured_page.page_url));
    md.push_str("## 一、问题总览\n\n");
    md.push_str("| # | 风险 | traceId | 接口 | 状态码 | 耗时 | 服务 | 日志信号 | SQL | EXPLAIN |\n");
    md.push_str("|---|------|---------|------|--------|------|------|----------|-----|---------|\n");

    for (idx, req) in captured_page.requests.iter().enumerate() {
        let trace_id = req.trace_id.as_deref().filter(|id| !id.is_empty());
        let request_logs = logs_for_trace(logs, trace_id);
        let request_sql = sql_for_trace(sql_traces, trace_id);
        let request_plans = plans_for_sqls(explain_plans, &request_sql);
        let parsed = resolve_request(req, gateway_prefix);
        let risk = classify_request_risk(req, &request_logs, &request_sql, &request_plans);
        let signal = format_log_signal(&request_logs);
        let explain = format_explain_status(&request_plans);
        let trace_label = trace_id.unwrap_or("无 traceId");

        md.push_str(&format!(
            "| {} | {} | `{}` | {} | {} | {}ms | {} | {} | {} | {} |\n",
            idx + 1,
            risk,
            trace_label,
            parsed.api_path,
            req.status,
            req.duration_ms,
            parsed.service,
            signal,
            request_sql.len(),
            explain,
        ));
    }

    md
}

struct RequestTarget {
    service: String,
    api_path: String,
}

fn resolve_request(req: &CapturedRequest, gateway_prefix: &str) -> RequestTarget {
    match url_resolver::resolve_url(&req.url, gateway_prefix) {
        Ok(resolved) => RequestTarget {
            service: resolved.service,
            api_path: resolved.api_path,
        },
        Err(_) => RequestTarget {
            service: "unknown".to_string(),
            api_path: req.url.clone(),
        },
    }
}

fn logs_for_trace<'a>(logs: &'a [LogEntry], trace_id: Option<&str>) -> Vec<&'a LogEntry> {
    match trace_id {
        Some(id) => logs
            .iter()
            .filter(|entry| entry.trace_id.as_deref() == Some(id))
            .collect(),
        None => Vec::new(),
    }
}

fn sql_for_trace<'a>(sql_traces: &'a [SqlTrace], trace_id: Option<&str>) -> Vec<&'a SqlTrace> {
    match trace_id {
        Some(id) => sql_traces
            .iter()
            .filter(|trace| trace.trace_id == id)
            .collect(),
        None => Vec::new(),
    }
}

fn plans_for_sqls<'a>(
    explain_plans: &'a [ExplainPlan],
    sql_traces: &[&SqlTrace],
) -> Vec<&'a ExplainPlan> {
    let trace_ids: HashSet<&str> = sql_traces
        .iter()
        .map(|trace| trace.trace_id.as_str())
        .collect();
    explain_plans
        .iter()
        .filter(|plan| {
            let fingerprint_match = sql_traces
                .iter()
                .any(|trace| trace.sql_fingerprint == plan.sql_fingerprint);
            let trace_match = plan
                .trace_id
                .as_deref()
                .map(|id| trace_ids.contains(id))
                .unwrap_or(true);
            fingerprint_match && trace_match
        })
        .collect()
}

fn classify_request_risk(
    req: &CapturedRequest,
    logs: &[&LogEntry],
    sql_traces: &[&SqlTrace],
    plans: &[&ExplainPlan],
) -> &'static str {
    let has_error_log = logs.iter().any(|entry| entry.level.eq_ignore_ascii_case("ERROR"));
    let has_warn_log = logs.iter().any(|entry| entry.level.eq_ignore_ascii_case("WARN"));
    let has_slow_sql = sql_traces
        .iter()
        .any(|trace| trace.duration_ms.map(|d| d > 1000.0).unwrap_or(false));
    let has_explain_error = plans.iter().any(|plan| plan.error.is_some());

    if req.status >= 500 || has_error_log {
        "ERROR"
    } else if req.duration_ms > 2000 || has_slow_sql {
        "SLOW"
    } else if req.status >= 400 || req.duration_ms > 1000 || has_warn_log || has_explain_error {
        "WARN"
    } else {
        "OK"
    }
}

fn format_log_signal(logs: &[&LogEntry]) -> String {
    let error_count = logs
        .iter()
        .filter(|entry| entry.level.eq_ignore_ascii_case("ERROR"))
        .count();
    let warn_count = logs
        .iter()
        .filter(|entry| entry.level.eq_ignore_ascii_case("WARN"))
        .count();
    format!("ERROR={} WARN={}", error_count, warn_count)
}

fn format_explain_status(plans: &[&ExplainPlan]) -> String {
    let success = plans.iter().filter(|plan| plan.error.is_none()).count();
    let failed = plans.iter().filter(|plan| plan.error.is_some()).count();
    format!("{} 成功 / {} 失败", success, failed)
}
```

- [ ] **Step 5: Run the overview test to verify it passes**

Run:

```bash
cargo test -p diag-core test_realtime_package_outputs_overview_index
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/diag-core/src/package.rs crates/diag-core/tests/integration.rs collector/src-tauri/src/diagnosis.rs
git commit -m "feat: add realtime diagnostic overview"
```

---

### Task 2: Add request-card report with request metadata and key logs

**Files:**
- Modify: `crates/diag-core/tests/integration.rs`
- Modify: `crates/diag-core/src/package.rs`

- [ ] **Step 1: Write the failing request-card log test**

Add this test after `test_realtime_package_outputs_overview_index`:

```rust
#[test]
fn test_realtime_package_outputs_request_cards_with_key_logs() {
    use std::io::Read;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("realtime-cards-log-test.zip");
    let (mut pkg, _) = mock_diagnosis_package();

    pkg.logs.push(LogEntry {
        time: Some("2026-06-03T12:00:04.000+08:00".into()),
        level: "INFO".into(),
        service: "pcm-management".into(),
        trace_id: Some("trace-orphan".into()),
        thread: None,
        class: None,
        method: None,
        message: "未匹配浏览器请求的后台日志".into(),
        exception: None,
        stack_trace: None,
        raw: "INFO orphan".into(),
    });

    build_realtime_package(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/gateway",
        &zip_path,
    )
    .unwrap();

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut cards = String::new();
    archive
        .by_name("realtime/request-cards.md")
        .expect("缺少实时请求排查卡片文件")
        .read_to_string(&mut cards)
        .unwrap();

    assert!(cards.contains("# 实时请求排查卡片"));
    assert!(cards.contains("## 1. [ERROR] x-trace：`trace-abc123`"));
    assert!(cards.contains("### 请求信息"));
    assert!(cards.contains("| Method / Status | GET / 200 |"));
    assert!(cards.contains("| duration | 3500 ms |"));
    assert!(cards.contains("| 入口服务 | pcm-management |"));
    assert!(cards.contains("### 初步判断"));
    assert!(cards.contains("- 结论：接口异常"));
    assert!(cards.contains("- 主要证据：ERROR 日志、慢请求"));
    assert!(cards.contains("### 关键日志"));
    assert!(cards.contains("Query timeout after 3000ms"));
    assert!(cards.contains("==>  Preparing: SELECT id, name, org_id FROM patient"));

    let first_card = cards
        .split("## 2. [OK] x-trace：`trace-def456`")
        .next()
        .unwrap();
    assert!(!first_card.contains("用户信息查询成功 userId=12345"));

    assert!(cards.contains("## 3. [ERROR] x-trace：`无 traceId`"));
    assert!(cards.contains("> 无 traceId，无法关联日志"));
    assert!(cards.contains("## 未匹配浏览器请求的日志"));
    assert!(cards.contains("trace-orphan"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
cargo test -p diag-core test_realtime_package_outputs_request_cards_with_key_logs
```

Expected: FAIL with missing `realtime/request-cards.md`.

- [ ] **Step 3: Write request-card file from `build_realtime_package`**

In `crates/diag-core/src/package.rs`, replace the `request-logs.md` write block with:

```rust
    let request_cards = render_realtime_request_cards_md(
        captured_page,
        logs,
        sql_traces,
        explain_plans,
        table_stats,
        gateway_prefix,
    );

    zip.start_file("realtime/request-cards.md", options)?;
    zip.write_all(request_cards.as_bytes())?;

    zip.start_file("realtime/request-logs.md", options)?;
    zip.write_all(request_cards.as_bytes())?;
```

Keeping `request-logs.md` as an alias preserves compatibility for existing consumers while making `request-cards.md` the new primary entry.

- [ ] **Step 4: Implement request-card metadata and key-log rendering**

Add this helper before `render_realtime_request_logs_md`:

```rust
fn render_realtime_request_cards_md(
    captured_page: &CapturedPage,
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    explain_plans: &[ExplainPlan],
    table_stats: &[TableStats],
    gateway_prefix: &str,
) -> String {
    let mut md = String::new();
    md.push_str("# 实时请求排查卡片\n\n");
    md.push_str(&format!("页面 URL：{}\n\n", captured_page.page_url));

    let captured_trace_ids: HashSet<&str> = captured_page
        .requests
        .iter()
        .filter_map(|req| req.trace_id.as_deref())
        .filter(|id| !id.is_empty())
        .collect();

    for (idx, req) in captured_page.requests.iter().enumerate() {
        let trace_id = req.trace_id.as_deref().filter(|id| !id.is_empty());
        let request_logs = logs_for_trace(logs, trace_id);
        let request_sql = sql_for_trace(sql_traces, trace_id);
        let request_plans = plans_for_sqls(explain_plans, &request_sql);
        let parsed = resolve_request(req, gateway_prefix);
        let risk = classify_request_risk(req, &request_logs, &request_sql, &request_plans);
        let trace_label = trace_id.unwrap_or("无 traceId");

        md.push_str(&format!(
            "## {}. [{}] x-trace：`{}`\n\n",
            idx + 1,
            risk,
            trace_label,
        ));
        md.push_str(&render_request_card_meta(req, &parsed));
        md.push_str(&render_request_diagnosis_summary(req, &request_logs, &request_sql, &request_plans));
        md.push_str(&render_request_key_logs(trace_id, &request_logs));
        md.push_str(&render_request_sql_cards(&request_sql, &request_plans, table_stats));
        md.push_str(&render_request_evidence_links(&request_logs, &request_sql));
        md.push_str("---\n\n");
    }

    let unmatched: Vec<&LogEntry> = logs
        .iter()
        .filter(|entry| {
            entry
                .trace_id
                .as_deref()
                .map(|id| !captured_trace_ids.contains(id))
                .unwrap_or(true)
        })
        .collect();

    if !unmatched.is_empty() {
        md.push_str("## 未匹配浏览器请求的日志\n\n```text\n");
        for entry in sort_log_refs(unmatched) {
            md.push_str(&format_log_line(entry));
            md.push('\n');
        }
        md.push_str("```\n\n");
    }

    md
}

fn render_request_card_meta(req: &CapturedRequest, target: &RequestTarget) -> String {
    let end_time = request_end_time(&req.timestamp, req.duration_ms);
    format!(
        "### 请求信息\n\n| 项目 | 值 |\n|------|-----|\n| Request URL | `{}` |\n| Method / Status | {} / {} |\n| startTime | {} |\n| endTime | {} |\n| duration | {} ms |\n| 入口服务 | {} |\n\n",
        req.url,
        req.method,
        req.status,
        req.timestamp,
        end_time,
        req.duration_ms,
        target.service,
    )
}

fn render_request_diagnosis_summary(
    req: &CapturedRequest,
    logs: &[&LogEntry],
    sql_traces: &[&SqlTrace],
    plans: &[&ExplainPlan],
) -> String {
    let conclusion = request_conclusion(req, logs);
    let evidence = request_evidence(req, logs, sql_traces, plans);
    let priority = request_priority(sql_traces, plans, logs);
    format!(
        "### 初步判断\n\n- 结论：{}\n- 主要证据：{}\n- 建议优先排查：{}\n\n",
        conclusion, evidence, priority
    )
}

fn request_conclusion(req: &CapturedRequest, logs: &[&LogEntry]) -> &'static str {
    if req.status >= 500 || logs.iter().any(|entry| entry.level.eq_ignore_ascii_case("ERROR")) {
        "接口异常"
    } else if req.duration_ms > 2000 {
        "慢请求"
    } else if req.status >= 400 || logs.iter().any(|entry| entry.level.eq_ignore_ascii_case("WARN")) {
        "日志告警"
    } else {
        "暂无明显异常"
    }
}

fn request_evidence(
    req: &CapturedRequest,
    logs: &[&LogEntry],
    sql_traces: &[&SqlTrace],
    plans: &[&ExplainPlan],
) -> String {
    let mut evidence = Vec::new();
    if req.status >= 500 {
        evidence.push(format!("HTTP {}", req.status));
    }
    if logs.iter().any(|entry| entry.level.eq_ignore_ascii_case("ERROR")) {
        evidence.push("ERROR 日志".to_string());
    }
    if logs.iter().any(|entry| entry.level.eq_ignore_ascii_case("WARN")) {
        evidence.push("WARN 日志".to_string());
    }
    if req.duration_ms > 2000 {
        evidence.push("慢请求".to_string());
    }
    if sql_traces
        .iter()
        .any(|trace| trace.duration_ms.map(|d| d > 1000.0).unwrap_or(false))
    {
        evidence.push("慢 SQL".to_string());
    }
    if plans.iter().any(|plan| plan.error.is_some()) {
        evidence.push("EXPLAIN 失败".to_string());
    }
    if evidence.is_empty() {
        "未发现明显异常信号".to_string()
    } else {
        evidence.join("、")
    }
}

fn request_priority(
    sql_traces: &[&SqlTrace],
    plans: &[&ExplainPlan],
    logs: &[&LogEntry],
) -> String {
    if plans.iter().any(|plan| plan.error.is_some()) {
        "SQL 执行计划失败原因".to_string()
    } else if !sql_traces.is_empty() {
        "相关 SQL、表数据量与索引".to_string()
    } else if logs.iter().any(|entry| entry.level.eq_ignore_ascii_case("ERROR")) {
        "异常日志和堆栈".to_string()
    } else {
        "请求状态、耗时和关联服务日志".to_string()
    }
}

fn render_request_key_logs(trace_id: Option<&str>, logs: &[&LogEntry]) -> String {
    let mut md = String::new();
    md.push_str("### 关键日志\n\n");
    if trace_id.is_none() {
        md.push_str("> 无 traceId，无法关联日志\n\n");
        return md;
    }
    if logs.is_empty() {
        md.push_str("> 未查询到该 traceId 的日志\n\n");
        return md;
    }

    let key_logs: Vec<&LogEntry> = logs
        .iter()
        .copied()
        .filter(|entry| is_key_log(entry))
        .collect();
    let display_logs = if key_logs.is_empty() {
        sort_log_refs(logs.to_vec())
    } else {
        sort_log_refs(key_logs)
    };

    md.push_str("```text\n");
    for entry in display_logs {
        md.push_str(&format_log_line(entry));
        md.push('\n');
        if let Some(stack) = entry.stack_trace.as_deref().filter(|s| !s.trim().is_empty()) {
            md.push_str(stack);
            md.push('\n');
        }
    }
    md.push_str("```\n\n");
    md
}

fn is_key_log(entry: &LogEntry) -> bool {
    let level = entry.level.as_str();
    let msg = entry.message.as_str();
    level.eq_ignore_ascii_case("ERROR")
        || level.eq_ignore_ascii_case("WARN")
        || entry.exception.is_some()
        || entry.stack_trace.is_some()
        || msg.contains("RequestUrl:")
        || msg.contains("==>  Preparing:")
        || msg.contains("==> Preparing:")
        || msg.contains("==> Parameters:")
        || msg.contains("<==      Total:")
}

fn sort_log_refs(mut logs: Vec<&LogEntry>) -> Vec<&LogEntry> {
    logs.sort_by(|a, b| {
        a.time
            .as_deref()
            .unwrap_or("")
            .cmp(b.time.as_deref().unwrap_or(""))
            .then_with(|| a.service.cmp(&b.service))
    });
    logs
}
```

- [ ] **Step 5: Temporarily stub SQL/evidence helpers used by the cards**

Add these minimal helpers below `render_request_key_logs`. Task 3 will replace `render_request_sql_cards` with full SQL/EXPLAIN rendering:

```rust
fn render_request_sql_cards(
    _sql_traces: &[&SqlTrace],
    _plans: &[&ExplainPlan],
    _table_stats: &[TableStats],
) -> String {
    "### 相关 SQL 与执行计划\n\n> 未匹配到该请求的 SQL\n\n".to_string()
}

fn render_request_evidence_links(logs: &[&LogEntry], sql_traces: &[&SqlTrace]) -> String {
    let mut services: Vec<&str> = logs
        .iter()
        .map(|entry| entry.service.as_str())
        .chain(sql_traces.iter().map(|trace| trace.service.as_str()))
        .collect();
    services.sort_unstable();
    services.dedup();

    let mut md = String::new();
    md.push_str("### 完整证据\n\n");
    if services.is_empty() {
        md.push_str("- 未关联到服务级原始日志\n\n");
        return md;
    }
    for service in services {
        md.push_str(&format!("- 完整服务日志：`{}.txt`\n", service));
        md.push_str(&format!("- 服务 SQL 报告：`{}_sql.md`\n", service));
    }
    md.push('\n');
    md
}
```

- [ ] **Step 6: Run the request-card log test**

Run:

```bash
cargo test -p diag-core test_realtime_package_outputs_request_cards_with_key_logs
```

Expected: PASS.

- [ ] **Step 7: Run the existing realtime grouping test**

Update the existing section split in `test_realtime_package_groups_logs_by_captured_request_trace_id` so it matches the new risk-prefixed heading:

```rust
    let abc_section = report
        .split("## 2. [OK] x-trace")
        .next()
        .expect("缺少第一个请求分组");
```

Run:

```bash
cargo test -p diag-core test_realtime_package_groups_logs_by_captured_request_trace_id
```

Expected: PASS because `realtime/request-logs.md` remains available as the request-card alias and does not contain `x-trace (x0)`.

- [ ] **Step 8: Commit**

```bash
git add crates/diag-core/src/package.rs crates/diag-core/tests/integration.rs
git commit -m "feat: add realtime request diagnostic cards"
```

---

### Task 3: Attach SQL, EXPLAIN, and table stats to request cards

**Files:**
- Modify: `crates/diag-core/tests/integration.rs`
- Modify: `crates/diag-core/src/package.rs`

- [ ] **Step 1: Write the failing SQL card test**

Add this test after `test_realtime_package_outputs_request_cards_with_key_logs`:

```rust
#[test]
fn test_realtime_request_cards_include_sql_explain_and_table_stats() {
    use std::io::Read;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("realtime-cards-sql-test.zip");
    let (mut pkg, _) = mock_diagnosis_package();

    pkg.sql_traces[0].duration_ms = Some(1523.0);
    pkg.sql_traces[0].parameters = Some("218713736305705076(String), ACTIVE(String)".into());
    pkg.explain_plans.push(ExplainPlan {
        sql_fingerprint: pkg.sql_traces[0].sql_fingerprint.clone(),
        avg_duration_ms: 1523.0,
        source: "log_sql_explain".into(),
        explain_rows: vec![ExplainRow {
            id: Some(1),
            select_type: Some("SIMPLE".into()),
            table: Some("patient".into()),
            access_type: Some("ref".into()),
            possible_keys: Some("idx_org_status".into()),
            key: Some("idx_org_status".into()),
            rows: Some(50),
            filtered: Some(100.0),
            extra: Some("Using where".into()),
        }],
        table_stats: None,
        trace_id: Some("trace-abc123".into()),
        executed_sql: Some("SELECT id, name, org_id FROM patient WHERE org_id = '218713736305705076' AND status = 'ACTIVE' ORDER BY create_time DESC".into()),
        error: None,
        found_in_schema: Some("pcm_db".into()),
    });

    build_realtime_package(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/gateway",
        &zip_path,
    )
    .unwrap();

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut cards = String::new();
    archive
        .by_name("realtime/request-cards.md")
        .unwrap()
        .read_to_string(&mut cards)
        .unwrap();

    assert!(cards.contains("### 相关 SQL 与执行计划"));
    assert!(cards.contains("#### SQL 1：pcm_db.patient"));
    assert!(cards.contains("| 参数状态 | 已拼装 |"));
    assert!(cards.contains("| EXPLAIN 状态 | 成功 |"));
    assert!(cards.contains("SELECT id, name, org_id FROM patient WHERE org_id = '218713736305705076'"));
    assert!(cards.contains("| pcm_db.patient | 180,000 | 524,288,000 bytes | 2 | `PRIMARY(id) UNIQUE`<br>`idx_org_status(org_id,status)` |"));
    assert!(cards.contains("**执行计划（log_sql_explain - 来自 schema: pcm_db）：**"));
    assert!(cards.contains("| id | select_type | table | type | possible_keys | key | rows | filtered | Extra |"));
}
```

- [ ] **Step 2: Write the failing EXPLAIN error test**

Add this test after the previous one:

```rust
#[test]
fn test_realtime_request_cards_show_explain_failure_reason() {
    use std::io::Read;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("realtime-cards-explain-error-test.zip");
    let (mut pkg, _) = mock_diagnosis_package();

    pkg.sql_traces[0].parameters = None;
    pkg.explain_plans.push(ExplainPlan {
        sql_fingerprint: pkg.sql_traces[0].sql_fingerprint.clone(),
        avg_duration_ms: 0.0,
        source: "log_sql_explain".into(),
        explain_rows: vec![],
        table_stats: None,
        trace_id: Some("trace-abc123".into()),
        executed_sql: Some(pkg.sql_traces[0].sql.clone()),
        error: Some("SQL 参数未完整拼装，仍包含 ? 占位符；请检查 Parameters 日志是否被采集到".into()),
        found_in_schema: None,
    });

    build_realtime_package(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/gateway",
        &zip_path,
    )
    .unwrap();

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut cards = String::new();
    archive
        .by_name("realtime/request-cards.md")
        .unwrap()
        .read_to_string(&mut cards)
        .unwrap();

    assert!(cards.contains("| 参数状态 | 参数缺失 |"));
    assert!(cards.contains("| EXPLAIN 状态 | 失败 |"));
    assert!(cards.contains("EXPLAIN 执行失败"));
    assert!(cards.contains("SQL 参数未完整拼装，仍包含 ? 占位符"));
}
```

- [ ] **Step 3: Run both tests to verify they fail**

Run:

```bash
cargo test -p diag-core test_realtime_request_cards_include_sql_explain_and_table_stats
cargo test -p diag-core test_realtime_request_cards_show_explain_failure_reason
```

Expected: both FAIL because `render_request_sql_cards` still emits only “未匹配到该请求的 SQL”.

- [ ] **Step 4: Replace the SQL-card stub with full rendering**

Replace the stubbed `render_request_sql_cards` in `crates/diag-core/src/package.rs` with:

```rust
fn render_request_sql_cards(
    sql_traces: &[&SqlTrace],
    plans: &[&ExplainPlan],
    table_stats: &[TableStats],
) -> String {
    let mut md = String::new();
    md.push_str("### 相关 SQL 与执行计划\n\n");
    if sql_traces.is_empty() {
        md.push_str("> 未匹配到该请求的 SQL\n\n");
        return md;
    }

    let mut stats_map: HashMap<&str, Vec<&TableStats>> = HashMap::new();
    for stat in table_stats {
        stats_map
            .entry(stat.table_name.as_str())
            .or_default()
            .push(stat);
    }

    for (idx, trace) in sql_traces.iter().enumerate() {
        let trace_plans: Vec<&ExplainPlan> = plans
            .iter()
            .copied()
            .filter(|plan| plan.sql_fingerprint == trace.sql_fingerprint)
            .collect();
        md.push_str(&render_request_sql_card(
            idx + 1,
            *trace,
            &trace_plans,
            &stats_map,
        ));
    }

    md
}

fn render_request_sql_card(
    idx: usize,
    trace: &SqlTrace,
    plans: &[&ExplainPlan],
    stats_map: &HashMap<&str, Vec<&TableStats>>,
) -> String {
    let title = trace
        .tables
        .first()
        .map(|table| display_table_name(table, stats_map))
        .unwrap_or_else(|| "SQL 查询".to_string());
    let executed_sql = plans
        .iter()
        .find_map(|plan| plan.executed_sql.clone())
        .unwrap_or_else(|| match &trace.parameters {
            Some(params) if !params.trim().is_empty() => {
                crate::sql_parser::substitute_mybatis_parameters(&trace.sql, params)
            }
            _ => trace.sql.clone(),
        });
    let explain_status = if plans.iter().any(|plan| plan.error.is_some()) {
        "失败"
    } else if plans.is_empty() {
        "无"
    } else {
        "成功"
    };

    let mut md = String::new();
    md.push_str(&format!("#### SQL {}：{}\n\n", idx, title));
    md.push_str("| 项目 | 值 |\n");
    md.push_str("|------|-----|\n");
    md.push_str(&format!("| traceId | `{}` |\n", trace.trace_id));
    md.push_str(&format!("| 服务 | {} |\n", trace.service));
    if let Some(ts) = &trace.timestamp {
        md.push_str(&format!("| 时间 | {} |\n", ts));
    }
    if let Some(duration) = trace.duration_ms {
        md.push_str(&format!("| 耗时 | {:.2} ms |\n", duration));
    }
    if !trace.tables.is_empty() {
        let tables: Vec<String> = trace
            .tables
            .iter()
            .map(|table| display_table_name(table, stats_map))
            .collect();
        md.push_str(&format!("| 涉及表 | {} |\n", tables.join(", ")));
    }
    md.push_str(&format!("| 参数状态 | {} |\n", parameter_status(trace, plans)));
    md.push_str(&format!("| EXPLAIN 状态 | {} |\n\n", explain_status));

    md.push_str("```sql\n");
    md.push_str(&executed_sql);
    md.push_str("\n```\n\n");

    md.push_str(&render_request_table_stats(&trace.tables, stats_map));
    md.push_str(&render_request_explain_plans(plans));
    md
}

fn parameter_status(trace: &SqlTrace, plans: &[&ExplainPlan]) -> &'static str {
    if plans.iter().any(|plan| {
        plan.error
            .as_deref()
            .map(|error| error.contains("参数未完整拼装") || error.contains("? 占位符"))
            .unwrap_or(false)
    }) {
        "参数缺失"
    } else if trace
        .parameters
        .as_deref()
        .map(|params| !params.trim().is_empty())
        .unwrap_or(false)
        || plans.iter().any(|plan| plan.executed_sql.is_some())
    {
        "已拼装"
    } else {
        "参数缺失"
    }
}

fn display_table_name(table: &str, stats_map: &HashMap<&str, Vec<&TableStats>>) -> String {
    match stats_map.get(table).and_then(|stats| stats.first()) {
        Some(stats) if !stats.schema.is_empty() => format!("{}.{}", stats.schema, table),
        _ => table.to_string(),
    }
}

fn render_request_table_stats(
    tables: &[String],
    stats_map: &HashMap<&str, Vec<&TableStats>>,
) -> String {
    if tables.is_empty() {
        return String::new();
    }

    let mut md = String::new();
    md.push_str("**表数据量与索引：**\n\n");
    md.push_str("| 表名 | 行数 | 数据大小 | 索引数 | 索引列表 |\n");
    md.push_str("|------|------|----------|--------|----------|\n");
    for table in tables {
        if let Some(stats_list) = stats_map.get(table.as_str()) {
            for stats in stats_list {
                let idx_list: Vec<String> = stats
                    .indexes
                    .iter()
                    .map(|index| {
                        let unique = if index.unique { " UNIQUE" } else { "" };
                        format!("`{}({}){}`", index.name, index.columns.join(","), unique)
                    })
                    .collect();
                let table_display = if stats.schema.is_empty() {
                    table.clone()
                } else {
                    format!("{}.{}", stats.schema, table)
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    table_display,
                    format_number(stats.row_count),
                    stats
                        .data_size_bytes
                        .map(|b| format!("{} bytes", format_number(b)))
                        .unwrap_or_else(|| "-".to_string()),
                    stats.indexes.len(),
                    if idx_list.is_empty() {
                        "-".to_string()
                    } else {
                        idx_list.join("<br>")
                    },
                ));
            }
        } else {
            md.push_str(&format!("| {} | - | - | - | - |\n", table));
        }
    }
    md.push('\n');
    md
}

fn render_request_explain_plans(plans: &[&ExplainPlan]) -> String {
    if plans.is_empty() {
        return "**执行计划：** 无（未匹配到 EXPLAIN 结果）\n\n".to_string();
    }

    let mut md = String::new();
    for plan in plans {
        let source_suffix = if let Some(schema) = &plan.found_in_schema {
            format!("{} - 来自 schema: {}", plan.source, schema)
        } else {
            plan.source.clone()
        };
        md.push_str(&format!("**执行计划（{}）：**\n\n", source_suffix));
        if let Some(err) = &plan.error {
            md.push_str(&format!("> ⚠ EXPLAIN 执行失败：`{}`\n\n", err));
        } else {
            md.push_str(&render_explain_plan_md(plan));
            md.push('\n');
        }
    }
    md
}
```

- [ ] **Step 5: Run both SQL-card tests**

Run:

```bash
cargo test -p diag-core test_realtime_request_cards_include_sql_explain_and_table_stats
cargo test -p diag-core test_realtime_request_cards_show_explain_failure_reason
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/diag-core/src/package.rs crates/diag-core/tests/integration.rs
git commit -m "feat: attach sql explain data to request cards"
```

---

### Task 4: Verify compatibility and full workspace behavior

**Files:**
- Modify only if tests reveal a bug directly caused by Tasks 1-3.

- [ ] **Step 1: Run focused diag-core package tests**

Run:

```bash
cargo test -p diag-core test_realtime_package_outputs_overview_index
cargo test -p diag-core test_realtime_package_outputs_request_cards_with_key_logs
cargo test -p diag-core test_realtime_request_cards_include_sql_explain_and_table_stats
cargo test -p diag-core test_realtime_request_cards_show_explain_failure_reason
cargo test -p diag-core test_realtime_package_groups_logs_by_captured_request_trace_id
cargo test -p diag-core test_quick_package_outputs_markdown_sql
cargo test -p diag-core test_quick_package_renders_table_stats_with_schema_name
```

Expected: all commands PASS.

- [ ] **Step 2: Run package-level workspace tests**

Run:

```bash
cargo test -p diag-core
cargo test -p smart-diag-collector
```

Expected: both commands PASS. Existing dead-code warnings in `smart-diag-collector` are acceptable only if there are no new failures.

- [ ] **Step 3: Run touched-file formatting check**

Run:

```bash
rustfmt --edition 2021 --check crates/diag-core/src/package.rs crates/diag-core/tests/integration.rs collector/src-tauri/src/diagnosis.rs
```

Expected: PASS.

- [ ] **Step 4: Run full workspace tests**

Run:

```bash
cargo test
```

Expected: PASS for all workspace unit, integration, and doc tests.

- [ ] **Step 5: Inspect ZIP file names manually from tests if a failure occurs**

If a ZIP filename assertion fails, run:

```bash
cargo test -p diag-core test_realtime_package_outputs_overview_index -- --nocapture
```

Expected: no output is required for success. If the test is changed to print archive names during debugging, remove that print before committing.

- [ ] **Step 6: Final commit**

```bash
git status --short
git add crates/diag-core/src/package.rs crates/diag-core/tests/integration.rs collector/src-tauri/src/diagnosis.rs
git commit -m "test: verify realtime diagnostic report format"
```

Skip this final commit if Task 4 produces no file changes after Task 3. Do not create an empty commit.

---

## Self-Review Checklist

- Spec coverage:
  - `realtime/overview.md`: Task 1.
  - `realtime/request-cards.md`: Task 2.
  - Request metadata, risk, logs, SQL, EXPLAIN, table stats in one card: Tasks 2 and 3.
  - No trace, no logs, EXPLAIN failure, unmatched logs: Tasks 2 and 3.
  - Preserve service raw files: Task 1 keeps `write_quick_contents`; Task 4 verifies quick package tests.
  - Gateway prefix from config: Task 1 updates function signature and collector call site.
- Type consistency:
  - `CapturedRequest`, `LogEntry`, `SqlTrace`, `ExplainPlan`, and `TableStats` fields match `crates/diag-core/src/models.rs`.
  - `build_realtime_package` call sites include `gateway_prefix: &str`.
  - Private helpers remain in `package.rs`; no public DTO changes are required.
- Testing:
  - Every behavior change starts with a failing integration test.
  - Focused tests run before broader package and workspace tests.
