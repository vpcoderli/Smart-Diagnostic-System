# Collector v2.0 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Upgrade the collector to support ELK log retrieval, Nacos service discovery, SQL extraction from logs with EXPLAIN, and a historical keyword-based collection mode.

**Architecture:** Add `LogCollector` and `ServiceDiscovery` traits in `diag-core` with ELK and Nacos implementations in the collector. Refactor `DiagnosisRunner` to accept trait objects, enabling ELK→SSH fallback. Add a new "historical mode" that drives collection from keyword search rather than WebView capture.

**Tech Stack:** Rust, Tauri 2.x, `reqwest` (HTTP client for ELK/Nacos), existing `sqlx`/`russh`/`diag-core` crates, vanilla HTML/JS/CSS frontend.

---

## Task 1: Add `reqwest` dependency and trait definitions in `diag-core`

**Files:**
- Modify: `Cargo.toml` (workspace root, add `reqwest` to workspace deps)
- Modify: `collector/src-tauri/Cargo.toml` (add `reqwest` dep)
- Create: `crates/diag-core/src/collector_trait.rs`
- Modify: `crates/diag-core/src/lib.rs` (add module)
- Modify: `crates/diag-core/src/models.rs` (add new types)

- [ ] **Step 1: Add `reqwest` to workspace dependencies**

In `Cargo.toml` (workspace root), add under `[workspace.dependencies]`:

```toml
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
```

In `collector/src-tauri/Cargo.toml`, add under `[dependencies]`:

```toml
reqwest = { workspace = true }
```

- [ ] **Step 2: Add new model types for v2**

In `crates/diag-core/src/models.rs`, add at the end:

```rust
// ─── v2: 时间窗口 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeWindow {
    pub start: String,
    pub end: String,
}

// ─── v2: 服务实例 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceInstance {
    pub service_name: String,
    pub ip: String,
    pub port: u16,
    pub healthy: bool,
    pub log_dir: String,
    pub log_pattern: String,
}

// ─── v2: SQL Trace（从日志中提取的 SQL）───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlTrace {
    pub trace_id: String,
    pub service: String,
    pub sql: String,
    pub sql_fingerprint: String,
    pub duration_ms: Option<f64>,
    pub tables: Vec<String>,
    pub timestamp: Option<String>,
}

// ─── v2: EXPLAIN 结果 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainPlan {
    pub sql_fingerprint: String,
    pub avg_duration_ms: f64,
    pub source: String,
    pub explain_rows: Vec<ExplainRow>,
    pub table_stats: Option<TableStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainRow {
    pub id: Option<i32>,
    pub select_type: Option<String>,
    pub table: Option<String>,
    pub access_type: Option<String>,
    pub possible_keys: Option<String>,
    pub key: Option<String>,
    pub rows: Option<i64>,
    pub filtered: Option<f64>,
    pub extra: Option<String>,
}
```

- [ ] **Step 3: Create `collector_trait.rs`**

Create `crates/diag-core/src/collector_trait.rs`:

```rust
use crate::models::{LogEntry, ServiceInstance, TimeWindow};
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait LogCollector: Send + Sync {
    async fn query_by_trace_ids(
        &self,
        trace_ids: &[String],
        service: Option<&str>,
        window: &TimeWindow,
    ) -> Result<Vec<LogEntry>>;

    async fn query_by_keywords(
        &self,
        keywords: &[String],
        service: Option<&str>,
        window: &TimeWindow,
    ) -> Result<Vec<LogEntry>>;

    fn source_type(&self) -> &'static str;
}

#[async_trait]
pub trait ServiceDiscovery: Send + Sync {
    async fn discover_services(&self, prefix: &str) -> Result<Vec<ServiceInstance>>;
    fn source_type(&self) -> &'static str;
}
```

- [ ] **Step 4: Register module in `lib.rs`**

In `crates/diag-core/src/lib.rs`, add:

```rust
pub mod collector_trait;
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo build -p diag-core`
Expected: compiles with no errors.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml collector/src-tauri/Cargo.toml crates/diag-core/src/collector_trait.rs crates/diag-core/src/lib.rs crates/diag-core/src/models.rs
git commit -m "feat: add LogCollector/ServiceDiscovery traits and v2 model types"
```

---

## Task 2: Implement ELK Log Collector

**Files:**
- Create: `collector/src-tauri/src/elk_collector.rs`
- Modify: `collector/src-tauri/src/lib.rs` (add module)

- [ ] **Step 1: Write test for ES version detection**

Create `collector/src-tauri/src/elk_collector.rs` with a unit test:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_es_version() {
        assert_eq!(parse_es_major_version("7.17.0"), Some(7));
        assert_eq!(parse_es_major_version("6.8.23"), Some(6));
        assert_eq!(parse_es_major_version("8.11.1"), Some(8));
        assert_eq!(parse_es_major_version("invalid"), None);
    }
}
```

- [ ] **Step 2: Implement ELK collector struct and version detection**

```rust
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use diag_core::collector_trait::LogCollector;
use diag_core::models::{LogEntry, TimeWindow};
use reqwest::Client;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct ElkConfig {
    pub address: String,
    pub index_pattern: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

pub struct ElkCollector {
    config: ElkConfig,
    client: Client,
    es_major_version: u8,
}

#[derive(Deserialize)]
struct ClusterInfo {
    version: EsVersion,
}

#[derive(Deserialize)]
struct EsVersion {
    number: String,
}

fn parse_es_major_version(version_str: &str) -> Option<u8> {
    version_str.split('.').next()?.parse().ok()
}

impl ElkCollector {
    pub async fn new(config: ElkConfig) -> Result<Self> {
        let mut builder = Client::builder();
        let client = builder.build()?;

        let url = format!("{}/", config.address.trim_end_matches('/'));
        let mut req = client.get(&url);
        if let (Some(u), Some(p)) = (&config.username, &config.password) {
            req = req.basic_auth(u, Some(p));
        }

        let resp = req.send().await
            .map_err(|e| anyhow!("ELK 连接失败: {}", e))?;
        let info: ClusterInfo = resp.json().await
            .map_err(|e| anyhow!("ELK 版本检测失败: {}", e))?;

        let es_major_version = parse_es_major_version(&info.version.number)
            .ok_or_else(|| anyhow!("无法解析 ES 版本: {}", info.version.number))?;

        tracing::info!("ELK 连接成功，版本: {} (major={})", info.version.number, es_major_version);

        Ok(Self { config, client, es_major_version })
    }
}
```

- [ ] **Step 3: Implement `LogCollector` trait for ELK**

```rust
#[async_trait]
impl LogCollector for ElkCollector {
    async fn query_by_trace_ids(
        &self,
        trace_ids: &[String],
        service: Option<&str>,
        window: &TimeWindow,
    ) -> Result<Vec<LogEntry>> {
        let mut all_entries = Vec::new();
        for trace_id in trace_ids {
            let body = self.build_trace_query(trace_id, service, window);
            let entries = self.execute_search(&body).await?;
            all_entries.extend(entries);
            if all_entries.len() >= 1000 {
                break;
            }
        }
        Ok(all_entries)
    }

    async fn query_by_keywords(
        &self,
        keywords: &[String],
        service: Option<&str>,
        window: &TimeWindow,
    ) -> Result<Vec<LogEntry>> {
        let query_str = keywords.join(" AND ");
        let body = self.build_keyword_query(&query_str, service, window);
        self.execute_search(&body).await
    }

    fn source_type(&self) -> &'static str {
        "elk"
    }
}
```

- [ ] **Step 4: Implement query builders and search execution**

```rust
impl ElkCollector {
    fn build_trace_query(
        &self,
        trace_id: &str,
        service: Option<&str>,
        window: &TimeWindow,
    ) -> serde_json::Value {
        let mut must = vec![
            serde_json::json!({ "term": { "traceId": trace_id } }),
            serde_json::json!({ "range": { "@timestamp": { "gte": window.start, "lte": window.end } } }),
        ];
        if let Some(svc) = service {
            must.push(serde_json::json!({ "term": { "serviceName": svc } }));
        }
        serde_json::json!({
            "query": { "bool": { "must": must } },
            "size": 1000,
            "sort": [{ "@timestamp": "asc" }]
        })
    }

    fn build_keyword_query(
        &self,
        query_str: &str,
        service: Option<&str>,
        window: &TimeWindow,
    ) -> serde_json::Value {
        let mut must: Vec<serde_json::Value> = vec![
            serde_json::json!({ "query_string": { "query": query_str } }),
            serde_json::json!({ "range": { "@timestamp": { "gte": window.start, "lte": window.end } } }),
        ];
        if let Some(svc) = service {
            must.push(serde_json::json!({ "term": { "serviceName": svc } }));
        }
        serde_json::json!({
            "query": { "bool": { "must": must } },
            "size": 5000,
            "sort": [{ "@timestamp": "asc" }]
        })
    }

    async fn execute_search(&self, body: &serde_json::Value) -> Result<Vec<LogEntry>> {
        let url = format!(
            "{}/{}/_search",
            self.config.address.trim_end_matches('/'),
            self.config.index_pattern
        );

        let mut req = self.client.post(&url).json(body);
        if let (Some(u), Some(p)) = (&self.config.username, &self.config.password) {
            req = req.basic_auth(u, Some(p));
        }

        let resp = req.send().await
            .map_err(|e| anyhow!("ELK 查询失败: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("ELK 返回错误 {}: {}", status, text));
        }

        let result: serde_json::Value = resp.json().await?;
        let hits = result["hits"]["hits"].as_array()
            .ok_or_else(|| anyhow!("ELK 响应格式异常"))?;

        let entries: Vec<LogEntry> = hits.iter().filter_map(|hit| {
            let source = hit.get("_source")?;
            Some(LogEntry {
                time: source.get("@timestamp").and_then(|t| t.as_str()).map(String::from),
                level: source.get("level").and_then(|l| l.as_str()).unwrap_or("UNKNOWN").to_string(),
                service: source.get("serviceName").and_then(|s| s.as_str()).unwrap_or("unknown").to_string(),
                trace_id: source.get("traceId").and_then(|t| t.as_str()).map(String::from),
                thread: source.get("thread").and_then(|t| t.as_str()).map(String::from),
                class: source.get("class").and_then(|c| c.as_str()).map(String::from),
                method: source.get("method").and_then(|m| m.as_str()).map(String::from),
                message: source.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string(),
                exception: source.get("exception").and_then(|e| e.as_str()).map(String::from),
                stack_trace: source.get("stackTrace").and_then(|s| s.as_str()).map(String::from),
                raw: serde_json::to_string(source).unwrap_or_default(),
            })
        }).collect();

        Ok(entries)
    }
}
```

- [ ] **Step 5: Register module in `lib.rs`**

In `collector/src-tauri/src/lib.rs`, add:

```rust
mod elk_collector;
```

- [ ] **Step 6: Verify it compiles**

Run: `cargo build -p smart-diag-collector`
Expected: compiles with no errors.

- [ ] **Step 7: Commit**

```bash
git add collector/src-tauri/src/elk_collector.rs collector/src-tauri/src/lib.rs
git commit -m "feat: implement ELK log collector with ES 6/7/8 version detection"
```

---

## Task 3: Implement Nacos Service Discovery

**Files:**
- Create: `collector/src-tauri/src/nacos_discovery.rs`
- Modify: `collector/src-tauri/src/lib.rs` (add module)

- [ ] **Step 1: Write test for log path inference**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_log_path() {
        assert_eq!(
            infer_log_path("pcm-management", "/var/log/{service-name}/"),
            "/var/log/pcm-management/"
        );
    }

    #[test]
    fn test_infer_log_path_no_placeholder() {
        assert_eq!(
            infer_log_path("pcm-management", "/data/logs/"),
            "/data/logs/"
        );
    }
}
```

- [ ] **Step 2: Implement Nacos discovery struct**

```rust
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use diag_core::collector_trait::ServiceDiscovery;
use diag_core::models::ServiceInstance;
use reqwest::Client;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct NacosConfig {
    pub address: String,
    pub namespace: String,
    pub group: String,
    pub service_prefix: String,
    pub log_path_pattern: String,
}

pub struct NacosDiscovery {
    config: NacosConfig,
    client: Client,
}

#[derive(Deserialize)]
struct NacosServiceList {
    doms: Option<Vec<String>>,
    services: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct NacosInstanceList {
    hosts: Vec<NacosInstance>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NacosInstance {
    ip: String,
    port: u16,
    healthy: bool,
    service_name: Option<String>,
}

fn infer_log_path(service_name: &str, pattern: &str) -> String {
    pattern.replace("{service-name}", service_name)
}

impl NacosDiscovery {
    pub fn new(config: NacosConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }
}
```

- [ ] **Step 3: Implement `ServiceDiscovery` trait**

```rust
#[async_trait]
impl ServiceDiscovery for NacosDiscovery {
    async fn discover_services(&self, prefix: &str) -> Result<Vec<ServiceInstance>> {
        let list_url = format!(
            "{}/nacos/v1/ns/service/list?pageNo=1&pageSize=100&namespaceId={}&groupName={}",
            self.config.address.trim_end_matches('/'),
            self.config.namespace,
            self.config.group
        );

        let resp = self.client.get(&list_url).send().await
            .map_err(|e| anyhow!("Nacos 连接失败: {}", e))?;
        let list: NacosServiceList = resp.json().await
            .map_err(|e| anyhow!("Nacos 服务列表解析失败: {}", e))?;

        let service_names: Vec<String> = list.doms.or(list.services)
            .unwrap_or_default()
            .into_iter()
            .filter(|name| name.starts_with(prefix))
            .collect();

        tracing::info!("Nacos 发现 {} 个 {} 服务", service_names.len(), prefix);

        let mut instances = Vec::new();
        for svc_name in &service_names {
            let inst_url = format!(
                "{}/nacos/v1/ns/instance/list?serviceName={}&namespaceId={}&groupName={}&healthyOnly=true",
                self.config.address.trim_end_matches('/'),
                svc_name,
                self.config.namespace,
                self.config.group
            );

            match self.client.get(&inst_url).send().await {
                Ok(resp) => {
                    if let Ok(inst_list) = resp.json::<NacosInstanceList>().await {
                        for inst in inst_list.hosts {
                            instances.push(ServiceInstance {
                                service_name: svc_name.clone(),
                                ip: inst.ip,
                                port: inst.port,
                                healthy: inst.healthy,
                                log_dir: infer_log_path(svc_name, &self.config.log_path_pattern),
                                log_pattern: "*.log".to_string(),
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Nacos 查询服务 {} 实例失败: {}", svc_name, e);
                }
            }
        }

        Ok(instances)
    }

    fn source_type(&self) -> &'static str {
        "nacos"
    }
}
```

- [ ] **Step 4: Register module in `lib.rs`**

In `collector/src-tauri/src/lib.rs`, add:

```rust
mod nacos_discovery;
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo build -p smart-diag-collector`
Expected: compiles with no errors.

- [ ] **Step 6: Commit**

```bash
git add collector/src-tauri/src/nacos_discovery.rs collector/src-tauri/src/lib.rs
git commit -m "feat: implement Nacos service discovery with log path inference"
```

---

## Task 4: Implement SQL Extractor (extract SQL from log lines)

**Files:**
- Create: `collector/src-tauri/src/sql_extractor.rs`
- Modify: `collector/src-tauri/src/lib.rs` (add module)

- [ ] **Step 1: Write tests for SQL extraction from log lines**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_mybatis_sql() {
        let line = "2026-05-26 10:00:00 DEBUG c.m.dao.PatientMapper - ==>  Preparing: SELECT id, name FROM patient WHERE org_id = ? AND status = ?";
        let result = extract_sql_from_line(line);
        assert!(result.is_some());
        let sql = result.unwrap();
        assert!(sql.starts_with("SELECT"));
    }

    #[test]
    fn test_extract_hibernate_sql() {
        let line = "Hibernate: select patient0_.id from patient patient0_ where patient0_.org_id=?";
        let result = extract_sql_from_line(line);
        assert!(result.is_some());
    }

    #[test]
    fn test_no_sql_in_line() {
        let line = "2026-05-26 10:00:00 INFO  c.m.service.PatientService - 查询患者列表完成";
        let result = extract_sql_from_line(line);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_sql_traces_from_logs() {
        let logs = vec![
            LogEntry {
                time: Some("2026-05-26T10:00:00".into()),
                level: "DEBUG".into(),
                service: "pcm-management".into(),
                trace_id: Some("abc123".into()),
                thread: None, class: None, method: None,
                message: "==>  Preparing: SELECT * FROM patient WHERE id = ?".into(),
                exception: None, stack_trace: None,
                raw: "".into(),
            },
        ];
        let traces = extract_sql_traces(&logs);
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].trace_id, "abc123");
        assert_eq!(traces[0].service, "pcm-management");
    }
}
```

- [ ] **Step 2: Implement SQL extraction logic**

```rust
use diag_core::models::{LogEntry, SqlTrace};
use diag_core::sql_parser;
use regex::Regex;
use std::sync::LazyLock;

static MYBATIS_SQL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)==>\s+Preparing:\s+(.+)$").unwrap()
});

static HIBERNATE_SQL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^Hibernate:\s+(.+)$").unwrap()
});

static GENERIC_SQL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(SELECT|INSERT|UPDATE|DELETE)\b.+\b(FROM|INTO|SET)\b.+").unwrap()
});

pub fn extract_sql_from_line(line: &str) -> Option<String> {
    if let Some(cap) = MYBATIS_SQL_REGEX.captures(line) {
        return Some(cap[1].trim().to_string());
    }
    if let Some(cap) = HIBERNATE_SQL_REGEX.captures(line) {
        return Some(cap[1].trim().to_string());
    }
    if GENERIC_SQL_REGEX.is_match(line) {
        if let Some(mat) = GENERIC_SQL_REGEX.find(line) {
            return Some(mat.as_str().trim().to_string());
        }
    }
    None
}

pub fn extract_sql_traces(logs: &[LogEntry]) -> Vec<SqlTrace> {
    let mut traces = Vec::new();

    for entry in logs {
        let trace_id = match &entry.trace_id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => continue,
        };

        let sql = match extract_sql_from_line(&entry.message) {
            Some(s) => s,
            None => {
                if let Some(s) = extract_sql_from_line(&entry.raw) {
                    s
                } else {
                    continue;
                }
            }
        };

        let fingerprint = sql_parser::fingerprint_sql(&sql);
        let tables = sql_parser::extract_tables(&sql);

        traces.push(SqlTrace {
            trace_id,
            service: entry.service.clone(),
            sql,
            sql_fingerprint: fingerprint,
            duration_ms: None,
            tables,
            timestamp: entry.time.clone(),
        });
    }

    traces
}
```

- [ ] **Step 3: Register module and verify**

Add `mod sql_extractor;` to `collector/src-tauri/src/lib.rs`.

Run: `cargo build -p smart-diag-collector`
Expected: compiles with no errors.

- [ ] **Step 4: Run tests**

Run: `cargo test -p smart-diag-collector sql_extractor`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add collector/src-tauri/src/sql_extractor.rs collector/src-tauri/src/lib.rs
git commit -m "feat: implement SQL extractor for MyBatis/Hibernate log lines"
```

---

## Task 5: Implement EXPLAIN Collector (tiered strategy)

**Files:**
- Create: `collector/src-tauri/src/explain_collector.rs`
- Modify: `collector/src-tauri/src/lib.rs` (add module)

- [ ] **Step 1: Write test for slow SQL threshold filtering**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_explain_slow_sql() {
        assert!(should_explain(1200.0, 500.0));
        assert!(!should_explain(300.0, 500.0));
        assert!(should_explain(500.1, 500.0));
    }
}
```

- [ ] **Step 2: Implement EXPLAIN collector**

```rust
use anyhow::{anyhow, Result};
use diag_core::config::DatabaseConfig;
use diag_core::models::{ExplainPlan, ExplainRow, SlowSqlItem, TableStats};

const MAX_EXPLAIN_COUNT: usize = 20;

fn should_explain(avg_duration_ms: f64, threshold_ms: f64) -> bool {
    avg_duration_ms > threshold_ms
}

pub struct ExplainCollector {
    config: DatabaseConfig,
    threshold_ms: f64,
}

impl ExplainCollector {
    pub fn new(config: DatabaseConfig, threshold_ms: f64) -> Self {
        Self { config, threshold_ms }
    }

    pub async fn collect_explain_plans(
        &self,
        slow_sqls: &[SlowSqlItem],
    ) -> Vec<ExplainPlan> {
        let candidates: Vec<&SlowSqlItem> = slow_sqls.iter()
            .filter(|s| should_explain(s.duration_ms, self.threshold_ms))
            .take(MAX_EXPLAIN_COUNT)
            .collect();

        if candidates.is_empty() {
            return Vec::new();
        }

        match self.config.db_type.as_str() {
            "mysql" => self.explain_mysql(&candidates).await,
            "postgresql" | "postgres" => self.explain_postgresql(&candidates).await,
            _ => {
                tracing::warn!("EXPLAIN 不支持数据库类型: {}", self.config.db_type);
                Vec::new()
            }
        }
    }

    async fn explain_mysql(&self, sqls: &[&SlowSqlItem]) -> Vec<ExplainPlan> {
        let url = format!(
            "mysql://{}:{}@{}:{}/{}",
            self.config.username, self.config.password,
            self.config.host, self.config.port, self.config.database
        );

        let pool = match sqlx::MySqlPool::connect(&url).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("EXPLAIN MySQL 连接失败: {}", e);
                return Vec::new();
            }
        };

        let mut plans = Vec::new();
        for sql_item in sqls {
            let explain_sql = format!("EXPLAIN {}", sql_item.sql_fingerprint);
            match sqlx::query_as::<_, MysqlExplainRow>(&explain_sql)
                .fetch_all(&pool).await
            {
                Ok(rows) => {
                    let explain_rows: Vec<ExplainRow> = rows.into_iter().map(|r| ExplainRow {
                        id: r.id,
                        select_type: r.select_type,
                        table: r.table,
                        access_type: r.type_field,
                        possible_keys: r.possible_keys,
                        key: r.key,
                        rows: r.rows,
                        filtered: r.filtered,
                        extra: r.extra,
                    }).collect();

                    plans.push(ExplainPlan {
                        sql_fingerprint: sql_item.sql_fingerprint.clone(),
                        avg_duration_ms: sql_item.duration_ms,
                        source: "mysql_explain".to_string(),
                        explain_rows,
                        table_stats: None,
                    });
                }
                Err(e) => {
                    tracing::warn!("EXPLAIN 执行失败 ({}): {}", sql_item.sql_fingerprint, e);
                }
            }
        }

        pool.close().await;
        plans
    }

    async fn explain_postgresql(&self, sqls: &[&SlowSqlItem]) -> Vec<ExplainPlan> {
        let url = format!(
            "postgres://{}:{}@{}:{}/{}",
            self.config.username, self.config.password,
            self.config.host, self.config.port, self.config.database
        );

        let pool = match sqlx::PgPool::connect(&url).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("EXPLAIN PostgreSQL 连接失败: {}", e);
                return Vec::new();
            }
        };

        let mut plans = Vec::new();
        for sql_item in sqls {
            let explain_sql = format!("EXPLAIN (FORMAT JSON) {}", sql_item.sql_fingerprint);
            match sqlx::query_scalar::<_, String>(&explain_sql)
                .fetch_one(&pool).await
            {
                Ok(json_str) => {
                    plans.push(ExplainPlan {
                        sql_fingerprint: sql_item.sql_fingerprint.clone(),
                        avg_duration_ms: sql_item.duration_ms,
                        source: "pg_explain".to_string(),
                        explain_rows: vec![ExplainRow {
                            id: None,
                            select_type: None,
                            table: None,
                            access_type: None,
                            possible_keys: None,
                            key: None,
                            rows: None,
                            filtered: None,
                            extra: Some(json_str),
                        }],
                        table_stats: None,
                    });
                }
                Err(e) => {
                    tracing::warn!("EXPLAIN 执行失败 ({}): {}", sql_item.sql_fingerprint, e);
                }
            }
        }

        pool.close().await;
        plans
    }
}

#[derive(sqlx::FromRow)]
struct MysqlExplainRow {
    id: Option<i32>,
    select_type: Option<String>,
    table: Option<String>,
    #[sqlx(rename = "type")]
    type_field: Option<String>,
    possible_keys: Option<String>,
    key: Option<String>,
    rows: Option<i64>,
    filtered: Option<f64>,
    #[sqlx(rename = "Extra")]
    extra: Option<String>,
}
```

- [ ] **Step 3: Register module and verify**

Add `mod explain_collector;` to `collector/src-tauri/src/lib.rs`.

Run: `cargo build -p smart-diag-collector`
Expected: compiles with no errors.

- [ ] **Step 4: Commit**

```bash
git add collector/src-tauri/src/explain_collector.rs collector/src-tauri/src/lib.rs
git commit -m "feat: implement tiered EXPLAIN collector for MySQL and PostgreSQL"
```

---

## Task 6: Refactor DiagnosisRunner to use LogCollector trait + add SSH adapter

**Files:**
- Create: `collector/src-tauri/src/ssh_log_collector.rs`
- Modify: `collector/src-tauri/src/diagnosis.rs`
- Modify: `collector/src-tauri/src/lib.rs` (add module)

- [ ] **Step 1: Create SSH adapter implementing LogCollector trait**

Create `collector/src-tauri/src/ssh_log_collector.rs`:

```rust
use anyhow::Result;
use async_trait::async_trait;
use diag_core::collector_trait::LogCollector;
use diag_core::config::{ServiceConfig, SshConfig};
use diag_core::models::{LogEntry, TimeWindow};
use diag_core::log_parser;

use crate::ssh_collector;

pub struct SshLogCollector {
    ssh_config: SshConfig,
    services: Vec<ServiceConfig>,
    max_log_lines: usize,
}

impl SshLogCollector {
    pub fn new(ssh_config: SshConfig, services: Vec<ServiceConfig>, max_log_lines: usize) -> Self {
        Self { ssh_config, services, max_log_lines }
    }

    fn find_service(&self, name: &str) -> Option<&ServiceConfig> {
        self.services.iter().find(|s| s.name == name)
    }
}

#[async_trait]
impl LogCollector for SshLogCollector {
    async fn query_by_trace_ids(
        &self,
        trace_ids: &[String],
        service: Option<&str>,
        _window: &TimeWindow,
    ) -> Result<Vec<LogEntry>> {
        let services_to_query: Vec<&ServiceConfig> = match service {
            Some(name) => self.find_service(name).into_iter().collect(),
            None => self.services.iter().collect(),
        };

        let mut all_entries = Vec::new();
        for svc in services_to_query {
            for host in &svc.hosts {
                for trace_id in trace_ids {
                    match ssh_collector::grep_remote_logs(
                        host, &self.ssh_config,
                        &svc.log_dir, &svc.log_pattern,
                        trace_id, self.max_log_lines,
                    ).await {
                        Ok(lines) => {
                            for line in &lines {
                                all_entries.push(log_parser::parse_log_line(line, &svc.name));
                            }
                        }
                        Err(e) => {
                            tracing::warn!("SSH 采集 {}:{} traceId={} 失败: {}", svc.name, host, trace_id, e);
                        }
                    }
                }
            }
        }
        Ok(all_entries)
    }

    async fn query_by_keywords(
        &self,
        keywords: &[String],
        service: Option<&str>,
        _window: &TimeWindow,
    ) -> Result<Vec<LogEntry>> {
        let services_to_query: Vec<&ServiceConfig> = match service {
            Some(name) => self.find_service(name).into_iter().collect(),
            None => self.services.iter().collect(),
        };

        let mut all_entries = Vec::new();
        for svc in services_to_query {
            for host in &svc.hosts {
                for keyword in keywords {
                    match ssh_collector::grep_remote_logs(
                        host, &self.ssh_config,
                        &svc.log_dir, &svc.log_pattern,
                        keyword, self.max_log_lines,
                    ).await {
                        Ok(lines) => {
                            for line in &lines {
                                all_entries.push(log_parser::parse_log_line(line, &svc.name));
                            }
                        }
                        Err(e) => {
                            tracing::warn!("SSH 关键字采集 {}:{} 失败: {}", svc.name, host, e);
                        }
                    }
                }
            }
        }
        Ok(all_entries)
    }

    fn source_type(&self) -> &'static str {
        "ssh"
    }
}
```

- [ ] **Step 2: Refactor DiagnosisRunner to accept `Box<dyn LogCollector>`**

Replace `collect_service_logs` in `collector/src-tauri/src/diagnosis.rs`. The `DiagnosisRunner` struct becomes:

```rust
pub struct DiagnosisRunner {
    config: CollectorConfig,
    captured: Option<CapturedPage>,
    log_collector: Box<dyn LogCollector>,
    trace_ids: Vec<String>,
}

impl DiagnosisRunner {
    pub fn new(
        config: CollectorConfig,
        captured: Option<CapturedPage>,
        log_collector: Box<dyn LogCollector>,
    ) -> Self {
        Self { config, captured, log_collector, trace_ids: Vec::new() }
    }

    pub fn new_historical(
        config: CollectorConfig,
        log_collector: Box<dyn LogCollector>,
        trace_ids: Vec<String>,
    ) -> Self {
        Self { config, captured: None, log_collector, trace_ids }
    }
}
```

Update `run()` to use `self.log_collector.query_by_trace_ids(...)` instead of calling `ssh_collector` directly. The trace_ids come from either `self.captured.requests` (realtime mode) or `self.trace_ids` (historical mode).

- [ ] **Step 3: Register module in `lib.rs`**

Add `mod ssh_log_collector;` to `collector/src-tauri/src/lib.rs`.

- [ ] **Step 4: Verify it compiles**

Run: `cargo build -p smart-diag-collector`
Expected: compiles with no errors.

- [ ] **Step 5: Commit**

```bash
git add collector/src-tauri/src/ssh_log_collector.rs collector/src-tauri/src/diagnosis.rs collector/src-tauri/src/lib.rs
git commit -m "refactor: DiagnosisRunner uses LogCollector trait, add SSH adapter"
```

---

## Task 7: Integrate SQL extraction + EXPLAIN into DiagnosisRunner

**Files:**
- Modify: `collector/src-tauri/src/diagnosis.rs`
- Modify: `crates/diag-core/src/models.rs` (add fields to `DiagnosisPackage`)
- Modify: `crates/diag-core/src/package.rs` (write new zip entries)

- [ ] **Step 1: Add `sql_traces` and `explain_plans` to `DiagnosisPackage`**

In `crates/diag-core/src/models.rs`, update `DiagnosisPackage`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosisPackage {
    pub manifest: DiagnosisManifest,
    pub captured_page: CapturedPage,
    pub logs: Vec<LogEntry>,
    pub slow_sqls: Vec<SlowSqlItem>,
    pub table_stats: Vec<TableStats>,
    pub sql_traces: Vec<SqlTrace>,
    pub explain_plans: Vec<ExplainPlan>,
    pub collection_report: Option<CollectionReport>,
}
```

- [ ] **Step 2: Update `DiagnosisManifest` with v2 fields**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosisManifest {
    pub diagnosis_id: String,
    pub site: String,
    pub system: String,
    pub created_at: String,
    pub page_url: String,
    pub request_count: usize,
    pub services: Vec<String>,
    pub trace_ids: Vec<String>,
    pub database_type: String,
    pub privacy_level: String,
    pub collector_version: String,
    #[serde(default)]
    pub collection_mode: Option<String>,
    #[serde(default)]
    pub log_source: Option<String>,
    #[serde(default)]
    pub keywords: Option<Vec<String>>,
    #[serde(default)]
    pub time_range: Option<TimeWindow>,
}
```

- [ ] **Step 3: Update `package.rs` to write `sql-trace.jsonl` and `explain-plans.json`**

In `crates/diag-core/src/package.rs`, add to `build_package()` after the slow-sql section:

```rust
// services/{service}/sql-trace.jsonl
let mut sql_traces_by_service: std::collections::HashMap<&str, Vec<&crate::models::SqlTrace>> = std::collections::HashMap::new();
for trace in &package.sql_traces {
    sql_traces_by_service.entry(trace.service.as_str()).or_default().push(trace);
}
for (svc, traces) in &sql_traces_by_service {
    let path = format!("services/{}/sql-trace.jsonl", svc);
    zip.start_file(&path, options)?;
    for t in traces {
        zip.write_all(serde_json::to_string(t)?.as_bytes())?;
        zip.write_all(b"\n")?;
    }
}

// database/explain-plans.json
if !package.explain_plans.is_empty() {
    zip.start_file("database/explain-plans.json", options)?;
    zip.write_all(serde_json::to_string_pretty(&package.explain_plans)?.as_bytes())?;
}
```

- [ ] **Step 4: Wire SQL extraction and EXPLAIN into `DiagnosisRunner::run()`**

After log collection step in `diagnosis.rs`, add:

```rust
// Step 3b: 从日志中提取 SQL
let sql_traces = crate::sql_extractor::extract_sql_traces(&all_logs);
tracing::info!("从日志中提取到 {} 条 SQL", sql_traces.len());

// Step 3c: 与 DB 慢日志交叉，对慢查询执行 EXPLAIN
let explain_collector = crate::explain_collector::ExplainCollector::new(
    self.config.database.clone(),
    500.0, // threshold_ms
);
let explain_plans = explain_collector.collect_explain_plans(&slow_sqls).await;
tracing::info!("获取到 {} 条 EXPLAIN 计划", explain_plans.len());
```

Then include `sql_traces` and `explain_plans` in the `DiagnosisPackage` construction.

- [ ] **Step 5: Fix all compilation errors from `DiagnosisPackage` field additions**

Update all call sites that construct `DiagnosisPackage` (in `diagnosis.rs` and the analyzer's `commands.rs`) to include the new fields with `Vec::new()` defaults where needed.

Run: `cargo build`
Expected: compiles with no errors.

- [ ] **Step 6: Commit**

```bash
git add crates/diag-core/src/models.rs crates/diag-core/src/package.rs collector/src-tauri/src/diagnosis.rs analyzer/src-tauri/src/commands.rs
git commit -m "feat: integrate SQL trace extraction and EXPLAIN plans into diagnosis pipeline"
```

---

## Task 8: Add v2 config types (ELK + Nacos settings)

**Files:**
- Modify: `crates/diag-core/src/config.rs` (add ELK/Nacos config structs)
- Modify: `collector/src-tauri/src/config_store.rs` (add site config export/import)
- Modify: `collector/src-tauri/src/deployment.rs` (add v2 fields to `DeploymentManifest`)

- [ ] **Step 1: Add ELK and Nacos config structs**

In `crates/diag-core/src/config.rs`, add:

```rust
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ElkConfig {
    pub address: String,
    pub index_pattern: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NacosConfig {
    pub address: String,
    pub namespace: String,
    pub group: String,
    pub service_prefix: String,
    pub log_path_pattern: String,
}
```

Add optional fields to `CollectorConfig`:

```rust
pub struct CollectorConfig {
    // ... existing fields ...
    pub elk: Option<ElkConfig>,
    pub nacos: Option<NacosConfig>,
}
```

- [ ] **Step 2: Add site config export/import to `config_store.rs`**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteConfig {
    pub version: u32,
    pub site_name: String,
    pub system: String,
    pub gateway_prefix: String,
    pub elk: Option<diag_core::config::ElkConfig>,
    pub nacos: Option<diag_core::config::NacosConfig>,
    pub manifest: DeploymentManifest,
}

pub fn export_site_config(path: &Path, config: &SiteConfig) -> Result<()> {
    let json = serde_json::to_string_pretty(config)?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn import_site_config(path: &Path) -> Result<SiteConfig> {
    let json = std::fs::read_to_string(path)?;
    let config: SiteConfig = serde_json::from_str(&json)?;
    Ok(config)
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build`
Expected: compiles with no errors.

- [ ] **Step 4: Commit**

```bash
git add crates/diag-core/src/config.rs collector/src-tauri/src/config_store.rs
git commit -m "feat: add ELK and Nacos config types, site config export/import"
```

---

## Task 9: Add historical mode Tauri commands

**Files:**
- Modify: `collector/src-tauri/src/commands.rs` (add new commands)
- Modify: `collector/src-tauri/src/lib.rs` (register commands)

- [ ] **Step 1: Add `discover_from_nacos` command**

```rust
#[tauri::command]
pub async fn discover_from_nacos(
    state: State<'_, AppState>,
    nacos_address: String,
    namespace: String,
    group: String,
    service_prefix: String,
    log_path_pattern: String,
) -> Result<Vec<diag_core::models::ServiceInstance>, String> {
    let config = crate::nacos_discovery::NacosConfig {
        address: nacos_address,
        namespace,
        group,
        service_prefix: service_prefix.clone(),
        log_path_pattern,
    };

    let discovery = crate::nacos_discovery::NacosDiscovery::new(config);
    use diag_core::collector_trait::ServiceDiscovery;
    discovery.discover_services(&service_prefix).await
        .map_err(|e| e.to_string())
}
```

- [ ] **Step 2: Add `start_historical_diagnosis` command**

```rust
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoricalDiagRequest {
    pub keywords: Vec<String>,
    pub time_start: String,
    pub time_end: String,
}

#[tauri::command]
pub async fn start_historical_diagnosis(
    state: State<'_, AppState>,
    request: HistoricalDiagRequest,
) -> Result<serde_json::Value, String> {
    let config = state.config.lock().unwrap().clone()
        .ok_or("请先完成配置".to_string())?;

    let window = diag_core::models::TimeWindow {
        start: request.time_start,
        end: request.time_end,
    };

    // Build log collector: try ELK first, fallback to SSH
    let log_collector: Box<dyn diag_core::collector_trait::LogCollector> =
        if let Some(elk_config) = &config.elk {
            match crate::elk_collector::ElkCollector::new(crate::elk_collector::ElkConfig {
                address: elk_config.address.clone(),
                index_pattern: elk_config.index_pattern.clone(),
                username: elk_config.username.clone(),
                password: elk_config.password.clone(),
            }).await {
                Ok(elk) => Box::new(elk),
                Err(e) => {
                    tracing::warn!("ELK 不可用，降级到 SSH: {}", e);
                    Box::new(crate::ssh_log_collector::SshLogCollector::new(
                        config.ssh.clone(),
                        config.services.clone(),
                        config.collector.max_log_lines,
                    ))
                }
            }
        } else {
            Box::new(crate::ssh_log_collector::SshLogCollector::new(
                config.ssh.clone(),
                config.services.clone(),
                config.collector.max_log_lines,
            ))
        };

    // Query by keywords first to get trace IDs
    use diag_core::collector_trait::LogCollector;
    let keyword_logs = log_collector
        .query_by_keywords(&request.keywords, None, &window).await
        .map_err(|e| e.to_string())?;

    // Extract unique trace IDs from keyword results
    let trace_ids: Vec<String> = keyword_logs.iter()
        .filter_map(|l| l.trace_id.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let runner = crate::diagnosis::DiagnosisRunner::new_historical(
        config, log_collector, trace_ids,
    );

    let result = runner.run().await.map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "outputPath": result.output_path,
        "serviceCount": result.service_results.len(),
        "mode": "historical"
    }))
}
```

- [ ] **Step 3: Register new commands in `lib.rs`**

Add to the `invoke_handler` macro in `collector/src-tauri/src/lib.rs`:

```rust
// 第四步：历史回溯模式
discover_from_nacos,
start_historical_diagnosis,
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo build -p smart-diag-collector`
Expected: compiles with no errors.

- [ ] **Step 5: Commit**

```bash
git add collector/src-tauri/src/commands.rs collector/src-tauri/src/lib.rs
git commit -m "feat: add historical diagnosis and Nacos discovery Tauri commands"
```

---

## Task 10: Frontend — add mode selection and historical input UI

**Files:**
- Modify: `collector/src/index.html` (add mode tabs, historical form)
- Modify: `collector/src/app.js` (add historical mode logic)
- Modify: `collector/src/styles.css` (style new elements)

- [ ] **Step 1: Add mode selector tabs to `index.html`**

After the stepper `<nav>`, before `<main>`, add:

```html
<!-- 采集模式选择 -->
<div class="mode-selector" id="mode-selector" style="display:none">
  <button class="mode-btn active" id="mode-realtime" onclick="switchMode('realtime')">🌐 实时复现</button>
  <button class="mode-btn" id="mode-historical" onclick="switchMode('historical')">🔍 历史回溯</button>
</div>
```

- [ ] **Step 2: Add historical mode input panel in Phase 3**

Inside `#phase-3`, add a new section before the existing WebView controls:

```html
<!-- 历史回溯模式 -->
<section class="card" id="historical-panel" style="display:none">
  <h2>🔍 历史问题回溯</h2>
  <div class="form-grid">
    <div class="form-group" style="grid-column: 1 / -1">
      <label>搜索关键字（错误信息、患者ID、接口路径等）</label>
      <input type="text" id="hist-keywords" placeholder="多个关键字用逗号分隔" />
    </div>
    <div class="form-group">
      <label>开始时间</label>
      <input type="datetime-local" id="hist-time-start" />
    </div>
    <div class="form-group">
      <label>结束时间</label>
      <input type="datetime-local" id="hist-time-end" />
    </div>
  </div>
  <button class="btn btn-primary" id="btn-hist-collect" onclick="startHistoricalDiagnosis()">开始采集</button>
  <div id="hist-status" class="status-msg"></div>
  <div id="hist-result" style="display:none"></div>
</section>
```

- [ ] **Step 3: Add JS logic for mode switching and historical diagnosis**

In `collector/src/app.js`, add:

```javascript
// ═══ Mode Selection ═══
let currentMode = 'realtime';

function switchMode(mode) {
  currentMode = mode;
  document.getElementById('mode-realtime').classList.toggle('active', mode === 'realtime');
  document.getElementById('mode-historical').classList.toggle('active', mode === 'historical');
  document.getElementById('historical-panel').style.display = mode === 'historical' ? '' : 'none';
  document.getElementById('realtime-panel').style.display = mode === 'realtime' ? '' : 'none';
}

// ═══ Historical Mode ═══
async function startHistoricalDiagnosis() {
  const keywords = document.getElementById('hist-keywords').value.trim();
  const timeStart = document.getElementById('hist-time-start').value;
  const timeEnd = document.getElementById('hist-time-end').value;

  if (!keywords) { showStatus('hist-status', '请输入搜索关键字', 'error'); return; }
  if (!timeStart || !timeEnd) { showStatus('hist-status', '请选择时间范围', 'error'); return; }

  showStatus('hist-status', '正在采集，请稍候...', 'info');
  document.getElementById('btn-hist-collect').disabled = true;

  try {
    const result = await invoke('start_historical_diagnosis', {
      request: {
        keywords: keywords.split(',').map(k => k.trim()).filter(Boolean),
        timeStart: new Date(timeStart).toISOString(),
        timeEnd: new Date(timeEnd).toISOString(),
      }
    });
    showStatus('hist-status', `采集完成！诊断包已保存到: ${result.outputPath}`, 'success');
    document.getElementById('hist-result').style.display = '';
    document.getElementById('hist-result').innerHTML = `
      <div class="result-card">
        <p>诊断包路径: <code>${result.outputPath}</code></p>
        <p>采集服务数: ${result.serviceCount}</p>
      </div>`;
  } catch (e) {
    showStatus('hist-status', `采集失败: ${e}`, 'error');
  } finally {
    document.getElementById('btn-hist-collect').disabled = false;
  }
}
```

- [ ] **Step 4: Wrap existing Phase 3 WebView controls with `id="realtime-panel"`**

In `index.html`, wrap the existing Phase 3 content (URL input, open browser button, etc.) with:

```html
<div id="realtime-panel">
  <!-- existing Phase 3 WebView content -->
</div>
```

- [ ] **Step 5: Show mode selector when entering Phase 3**

In `app.js`, update `goPhase3()`:

```javascript
function goPhase3() {
  invoke('confirm_validation', {}).catch(() => {});
  showPhase(3);
  document.getElementById('mode-selector').style.display = '';
  switchMode('realtime');
}
```

- [ ] **Step 6: Add CSS for mode selector**

In `collector/src/styles.css`, add:

```css
.mode-selector {
  display: flex; gap: 8px; padding: 12px 24px;
  background: var(--bg-secondary); border-bottom: 1px solid var(--border);
}
.mode-btn {
  padding: 8px 16px; border: 1px solid var(--border); border-radius: 6px;
  background: transparent; cursor: pointer; font-size: 14px; color: var(--text-secondary);
  transition: all 0.2s;
}
.mode-btn.active {
  background: var(--primary); color: white; border-color: var(--primary);
}
.mode-btn:hover:not(.active) { background: var(--bg-hover); }
```

- [ ] **Step 7: Verify app launches**

Run: `cd collector/src-tauri && cargo tauri dev`
Expected: app opens, mode selector visible in Phase 3.

- [ ] **Step 8: Commit**

```bash
git add collector/src/index.html collector/src/app.js collector/src/styles.css
git commit -m "feat: add dual-mode UI with historical keyword-based collection"
```

---

## Task 11: Add Nacos discovery UI to Phase 1

**Files:**
- Modify: `collector/src/index.html` (add Nacos config section)
- Modify: `collector/src/app.js` (add Nacos discovery logic)

- [ ] **Step 1: Add Nacos config section in Phase 1**

In `index.html`, after the site info `<section>`, add:

```html
<section class="card">
  <h2>🔌 服务发现（Nacos）</h2>
  <p class="hint">从 Nacos 注册中心自动发现服务实例，可选填</p>
  <div class="form-grid">
    <div class="form-group">
      <label>Nacos 地址</label>
      <input type="text" id="nacos-address" placeholder="http://172.29.60.200:8848" />
    </div>
    <div class="form-group">
      <label>命名空间</label>
      <input type="text" id="nacos-namespace" placeholder="pcm-prod" />
    </div>
    <div class="form-group">
      <label>服务前缀</label>
      <input type="text" id="nacos-prefix" value="pcm-" />
    </div>
    <div class="form-group">
      <label>日志路径规则</label>
      <input type="text" id="nacos-log-pattern" value="/var/log/{service-name}/" />
    </div>
  </div>
  <button class="btn btn-outline" id="btn-nacos-discover" onclick="discoverFromNacos()">🔍 自动发现服务</button>
  <div id="nacos-status" class="status-msg"></div>
  <div id="nacos-result" style="display:none"></div>
</section>
```

- [ ] **Step 2: Add Nacos discovery JS**

```javascript
async function discoverFromNacos() {
  const address = document.getElementById('nacos-address').value.trim();
  if (!address) { showStatus('nacos-status', '请输入 Nacos 地址', 'error'); return; }

  showStatus('nacos-status', '正在连接 Nacos...', 'info');
  try {
    const instances = await invoke('discover_from_nacos', {
      nacosAddress: address,
      namespace: document.getElementById('nacos-namespace').value.trim() || 'public',
      group: 'DEFAULT_GROUP',
      servicePrefix: document.getElementById('nacos-prefix').value.trim() || 'pcm-',
      logPathPattern: document.getElementById('nacos-log-pattern').value.trim() || '/var/log/{service-name}/',
    });

    showStatus('nacos-status', `发现 ${instances.length} 个服务实例`, 'success');
    document.getElementById('nacos-result').style.display = '';
    document.getElementById('nacos-result').innerHTML = instances.map(i =>
      `<div class="instance-item">${i.serviceName} → ${i.ip}:${i.port} (${i.logDir})</div>`
    ).join('');

    svcImported = true;
  } catch (e) {
    showStatus('nacos-status', `发现失败: ${e}`, 'error');
  }
}
```

- [ ] **Step 3: Add ELK config section in Phase 1**

```html
<section class="card">
  <h2>📊 日志源（ELK）</h2>
  <p class="hint">配置 Elasticsearch 地址用于日志采集，可选填（不填则使用 SSH）</p>
  <div class="form-grid">
    <div class="form-group">
      <label>ES 地址</label>
      <input type="text" id="elk-address" placeholder="http://172.29.60.100:9200" />
    </div>
    <div class="form-group">
      <label>索引名</label>
      <input type="text" id="elk-index" value="app-logs-*" />
    </div>
    <div class="form-group">
      <label>用户名（可选）</label>
      <input type="text" id="elk-username" placeholder="" />
    </div>
    <div class="form-group">
      <label>密码（可选）</label>
      <input type="password" id="elk-password" placeholder="" />
    </div>
  </div>
</section>
```

- [ ] **Step 4: Verify app launches**

Run: `cd collector/src-tauri && cargo tauri dev`
Expected: Phase 1 shows Nacos and ELK config sections.

- [ ] **Step 5: Commit**

```bash
git add collector/src/index.html collector/src/app.js
git commit -m "feat: add Nacos discovery and ELK config UI in Phase 1"
```

---

## Task 12: End-to-end integration test + final wiring

**Files:**
- Modify: `collector/src-tauri/src/commands.rs` (wire ELK config into `start_diagnosis`)
- Modify: `collector/src-tauri/src/diagnosis.rs` (ensure realtime mode uses ELK→SSH fallback)

- [ ] **Step 1: Update `start_diagnosis` to use ELK when available**

In `commands.rs`, update the existing `start_diagnosis` command to construct the log collector the same way as `start_historical_diagnosis`:

```rust
let log_collector: Box<dyn diag_core::collector_trait::LogCollector> =
    if let Some(elk_config) = &config.elk {
        match crate::elk_collector::ElkCollector::new(/* ... */).await {
            Ok(elk) => {
                tracing::info!("使用 ELK 采集日志");
                Box::new(elk)
            }
            Err(e) => {
                tracing::warn!("ELK 不可用 ({}), 降级 SSH", e);
                Box::new(crate::ssh_log_collector::SshLogCollector::new(
                    config.ssh.clone(), config.services.clone(), config.collector.max_log_lines,
                ))
            }
        }
    } else {
        Box::new(crate::ssh_log_collector::SshLogCollector::new(
            config.ssh.clone(), config.services.clone(), config.collector.max_log_lines,
        ))
    };

let runner = DiagnosisRunner::new(config, Some(captured), log_collector);
```

- [ ] **Step 2: Run full workspace build**

Run: `cargo build`
Expected: all 3 crates compile without errors.

- [ ] **Step 3: Run all tests**

Run: `cargo test`
Expected: all existing + new tests pass.

- [ ] **Step 4: Commit**

```bash
git add collector/src-tauri/src/commands.rs collector/src-tauri/src/diagnosis.rs
git commit -m "feat: wire ELK fallback into realtime mode, complete v2 integration"
```

---

## Summary

| Task | Module | What it delivers |
|------|--------|-----------------|
| 1 | diag-core | Traits + new model types |
| 2 | collector | ELK log collector implementation |
| 3 | collector | Nacos service discovery |
| 4 | collector | SQL extraction from log lines |
| 5 | collector | EXPLAIN collector (tiered) |
| 6 | collector | SSH adapter + DiagnosisRunner refactor |
| 7 | both | SQL trace + EXPLAIN into package format |
| 8 | both | Config types for ELK/Nacos |
| 9 | collector | Historical mode Tauri commands |
| 10 | frontend | Dual-mode UI |
| 11 | frontend | Nacos/ELK config UI |
| 12 | collector | End-to-end wiring + tests |
