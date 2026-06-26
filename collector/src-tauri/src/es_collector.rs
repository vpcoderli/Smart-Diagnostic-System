use anyhow::{anyhow, Result};
use async_trait::async_trait;
use diag_core::collector_trait::LogCollector;
use diag_core::config::EsConfig;
use diag_core::models::{LogEntry, TimeWindow};
use elasticsearch::{Elasticsearch, SearchParts};
use elasticsearch::http::transport::{TransportBuilder, SingleNodeConnectionPool};
use elasticsearch::auth::Credentials;
use elasticsearch::cert::CertificateValidation;
use serde_json::json;

/// 构建对字符串字段（traceId / serviceName 等）的健壮精确匹配子句。
///
/// ES / Logstash 动态映射下字符串通常索引为 `text`（并带 `.keyword` 子字段），
/// 直接用 `term` 查 `text` 字段会因分词而匹配不到任何文档。用 `should` 覆盖三种映射：
/// keyword（`term`）、text+keyword 子字段（`term` on `<field>.keyword`）、纯 text（`match_phrase`）。
fn string_match_clause(field: &str, value: &str) -> serde_json::Value {
    json!({
        "bool": {
            "should": [
                { "term": { field: value } },
                { "term": { format!("{}.keyword", field): value } },
                { "match_phrase": { field: value } }
            ],
            "minimum_should_match": 1
        }
    })
}

/// traceId 候选字段：配置映射的 trace_id 字段之外，并入医院侧常见别名（`x0` 为实际字段）。
fn trace_id_field_candidates(configured: &str) -> Vec<String> {
    let mut fields = vec![configured.to_string()];
    for alias in ["x0", "traceId", "trace_id", "tid"] {
        if !fields.iter().any(|f| f == alias) {
            fields.push(alias.to_string());
        }
    }
    fields
}

/// traceId 专用匹配子句。对每个候选字段（配置字段 + `x0` 等别名）都尝试
/// term / term(.keyword) / match_phrase，再加一条 Kibana 式全文检索（跨所有字段搜原值），
/// 适配 traceId 存放在 `x0` 字段、或内嵌在 `msg` 文本的情况——等价于在 Kibana 直接搜该 traceId。
fn trace_match_clause(field: &str, value: &str) -> serde_json::Value {
    let mut should: Vec<serde_json::Value> = Vec::new();
    for f in trace_id_field_candidates(field) {
        should.push(json!({ "term": { f.clone(): value } }));
        should.push(json!({ "term": { format!("{}.keyword", f): value } }));
        should.push(json!({ "match_phrase": { f: value } }));
    }
    should.push(json!({
        "query_string": { "query": format!("\"{}\"", value), "default_operator": "AND" }
    }));
    json!({ "bool": { "should": should, "minimum_should_match": 1 } })
}

/// 按「配置字段名 + 常见别名」顺序提取字符串字段，返回第一个非空值。
/// 兼容医院侧实际字段名与默认映射不一致（如正文字段是 `msg` 而非 `message`）。
fn extract_with_aliases<'a>(
    source: &'a serde_json::Value,
    configured: &str,
    aliases: &[&str],
) -> Option<&'a str> {
    std::iter::once(configured)
        .chain(aliases.iter().copied())
        .filter_map(|key| source.get(key).and_then(|v| v.as_str()))
        .find(|s| !s.is_empty())
}

/// 提取 traceId（x-trace）。医院侧确认 traceId 存放在专用字段 `x0`（值即 x-trace），
/// 而 ES 文档另有一个内部 `traceId` 字段（点分链路 id，并非 x-trace）。必须让 `x0` 优先，
/// 否则会取到内部 id，导致按 x-trace 关联日志/SQL 失败。`x0` 缺失时回退到配置字段及其它别名。
fn extract_trace_id<'a>(source: &'a serde_json::Value, configured: &str) -> Option<&'a str> {
    extract_with_aliases(source, "x0", &[configured, "traceId", "trace_id", "tid"])
}

/// 提取服务名。医院侧实际字段为 `app`，默认映射 `serviceName` 取不到会落到 `services/unknown/`。
fn extract_service<'a>(source: &'a serde_json::Value, configured: &str) -> Option<&'a str> {
    extract_with_aliases(
        source,
        configured,
        &["app", "serviceName", "service", "application", "appName"],
    )
}

/// ES 直接连接日志采集器
pub struct EsCollector {
    config: EsConfig,
    client: Elasticsearch,
}

impl EsCollector {
    pub async fn new(config: EsConfig) -> Result<Self> {
        let url_parsed = url::Url::parse(&config.address)
            .map_err(|e| anyhow!("无效的 ES 地址 '{}': {}", config.address, e))?;

        let conn_pool = SingleNodeConnectionPool::new(url_parsed);
        let mut builder = TransportBuilder::new(conn_pool);

        if let (Some(u), Some(p)) = (&config.username, &config.password) {
            if !u.trim().is_empty() {
                builder = builder.auth(Credentials::Basic(u.clone(), p.clone()));
            }
        }
        builder = builder.cert_validation(CertificateValidation::None);
        let transport = builder.build().map_err(|e| anyhow!("构建 ES Transport 失败: {}", e))?;
        let client = Elasticsearch::new(transport);

        Ok(Self { config, client })
    }

    pub async fn get_es_version(&self) -> Result<String> {
        let response = self.client
            .info()
            .send()
            .await
            .map_err(|e| anyhow!("ES info 请求失败: {}", e))?;

        if !response.status_code().is_success() {
            return Err(anyhow!("ES info 返回错误状态: {}", response.status_code()));
        }

        let body = response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| anyhow!("解析 ES info 响应失败: {}", e))?;

        let version = body["version"]["number"]
            .as_str()
            .ok_or_else(|| anyhow!("ES 响应缺失 version.number 字段"))?
            .to_string();

        Ok(version)
    }

    fn build_trace_query(&self, trace_id: &str, service: Option<&str>, window: &TimeWindow) -> serde_json::Value {
        let mut must = vec![
            trace_match_clause(&self.config.field_mapping.trace_id, trace_id),
        ];
        if !window.start.is_empty() && !window.end.is_empty() {
            must.push(json!({ "range": {
                self.config.field_mapping.timestamp.clone(): {
                    "gte": window.start, "lte": window.end
                }
            }}));
        }
        if let Some(svc) = service {
            must.push(string_match_clause(&self.config.field_mapping.service, svc));
        }
        json!({
            "query": { "bool": { "must": must } },
            "size": self.config.max_hits_per_trace,
            "sort": [{ self.config.field_mapping.timestamp.clone(): "asc" }]
        })
    }

    fn build_keyword_query(&self, query_str: &str, service: Option<&str>, window: &TimeWindow) -> serde_json::Value {
        let mut must = vec![
            json!({
                "query_string": {
                    "query": query_str,
                    "analyze_wildcard": true,
                    "default_operator": "AND"
                }
            }),
            json!({ "range": {
                self.config.field_mapping.timestamp.clone(): {
                    "gte": window.start, "lte": window.end
                }
            }}),
        ];
        if let Some(svc) = service {
            must.push(
                json!({ "term": { self.config.field_mapping.service.clone(): svc } }),
            );
        }
        json!({
            "query": { "bool": { "must": must } },
            "size": 5000,
            "sort": [{ self.config.field_mapping.timestamp.clone(): "asc" }]
        })
    }

    async fn execute_search(&self, body: &serde_json::Value) -> Result<Vec<LogEntry>> {
        let response = self.client
            .search(SearchParts::Index(&[&self.config.index_pattern]))
            .body(body)
            .send()
            .await
            .map_err(|e| anyhow!("ES 查询执行失败: {}", e))?;

        if !response.status_code().is_success() {
            let status = response.status_code();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("ES 返回错误 {}: {}", status, text));
        }

        let result = response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| anyhow!("ES 响应解析失败: {}", e))?;

        if let Some(err_obj) = result.get("error") {
            let reason = err_obj
                .get("reason")
                .and_then(|r| r.as_str())
                .or_else(|| err_obj.as_str())
                .unwrap_or("未知错误");
            let err_type = err_obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
            return Err(anyhow!("ES 错误 [{}]: {}", err_type, reason));
        }

        let hits = result["hits"]["hits"].as_array().ok_or_else(|| {
            anyhow!("ES 响应结构异常（缺失 hits.hits）")
        })?;

        let entries: Vec<LogEntry> = hits
            .iter()
            .filter_map(|hit| {
                let source = hit.get("_source")?;
                Some(LogEntry {
                    time: source
                        .get(&self.config.field_mapping.timestamp)
                        .and_then(|t| t.as_str())
                        .map(String::from),
                    level: source
                        .get(&self.config.field_mapping.level)
                        .and_then(|l| l.as_str())
                        .unwrap_or("UNKNOWN")
                        .to_string(),
                    service: extract_service(source, &self.config.field_mapping.service)
                        .unwrap_or("unknown")
                        .to_string(),
                    trace_id: extract_trace_id(source, &self.config.field_mapping.trace_id)
                        .map(String::from),
                    thread: source
                        .get(&self.config.field_mapping.thread)
                        .and_then(|t| t.as_str())
                        .map(String::from),
                    class: source
                        .get("class")
                        .and_then(|c| c.as_str())
                        .map(String::from),
                    method: source
                        .get("method")
                        .and_then(|m| m.as_str())
                        .map(String::from),
                    message: extract_with_aliases(
                        source,
                        &self.config.field_mapping.message,
                        &["msg", "message", "content", "log_message"],
                    ).unwrap_or("").to_string(),
                    exception: source
                        .get(&self.config.field_mapping.exception)
                        .and_then(|e| e.as_str())
                        .map(String::from),
                    stack_trace: source
                        .get(&self.config.field_mapping.stack_trace)
                        .and_then(|s| s.as_str())
                        .map(String::from),
                    raw: serde_json::to_string(source).unwrap_or_default(),
                })
            })
            .collect();

        Ok(entries)
    }
}

#[async_trait]
impl LogCollector for EsCollector {
    async fn query_by_trace_ids(
        &self,
        trace_ids: &[String],
        service: Option<&str>,
        window: &TimeWindow,
    ) -> Result<Vec<LogEntry>> {
        if trace_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut merged = Vec::new();
        for trace_id in trace_ids {
            let body = self.build_trace_query(trace_id, service, window);
            merged.extend(self.execute_search(&body).await?);
        }
        merged.sort_by(|left, right| left.time.cmp(&right.time));
        Ok(merged)
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
        "es"
    }
}
