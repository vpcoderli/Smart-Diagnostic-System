use anyhow::{anyhow, Result};
use async_trait::async_trait;
use diag_core::collector_trait::LogCollector;
use diag_core::config::ElkConfig;
use diag_core::models::{LogEntry, TimeWindow};
use reqwest::Client;
use serde::Deserialize;

/// 连接模式：直连 ES 或通过 Kibana 代理
#[derive(Debug, Clone, PartialEq)]
enum ConnMode {
    /// 直接连接 Elasticsearch API（address 即 ES 根地址）
    DirectEs,
    /// 通过 Kibana 内置 ES 代理（address 为 Kibana 根地址，如 /kibana）
    KibanaProxy,
}

pub struct ElkCollector {
    config: ElkConfig,
    client: Client,
    es_major_version: u8,
    /// 连接模式（自动检测）
    mode: ConnMode,
}

#[derive(Deserialize)]
struct ClusterInfo {
    version: EsVersion,
}

#[derive(Deserialize)]
struct EsVersion {
    number: String,
}

/// Kibana /api/status 响应（简化，只读 version）
#[derive(Deserialize)]
struct KibanaStatus {
    version: Option<KibanaVersion>,
}
#[derive(Deserialize)]
struct KibanaVersion {
    number: Option<String>,
}

fn parse_es_major_version(version_str: &str) -> Option<u8> {
    version_str.split('.').next()?.parse().ok()
}

impl ElkCollector {
    /// 创建 ElkCollector，自动检测连接模式：
    /// 1. 先尝试直连 ES（GET {address}/）
    /// 2. 失败则尝试 Kibana 代理（GET {address}/api/status）
    pub async fn new(config: ElkConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(true) // 允许自签证书（医院内网常见）
            .build()?;

        let base = config.address.trim_end_matches('/');

        // ── 尝试1：直连 ES ──
        let es_url = format!("{}/", base);
        let mut req = client.get(&es_url);
        if let (Some(u), Some(p)) = (&config.username, &config.password) {
            req = req.basic_auth(u, Some(p));
        }

        if let Ok(resp) = req.send().await {
            if let Ok(info) = resp.json::<ClusterInfo>().await {
                if let Some(ver) = parse_es_major_version(&info.version.number) {
                    tracing::info!(
                        "直连 ES 成功，版本: {} (major={})",
                        info.version.number,
                        ver
                    );
                    return Ok(Self {
                        config,
                        client,
                        es_major_version: ver,
                        mode: ConnMode::DirectEs,
                    });
                }
            }
        }

        // ── 尝试2：Kibana 代理模式 ──
        // Kibana 在 {address}/api/status 返回状态
        let kibana_status_url = format!("{}/api/status", base);
        let mut req2 = client.get(&kibana_status_url).header("kbn-xsrf", "true");
        if let (Some(u), Some(p)) = (&config.username, &config.password) {
            req2 = req2.basic_auth(u, Some(p));
        }

        match req2.send().await {
            Ok(resp) if resp.status().is_success() => {
                let ver_str = resp
                    .json::<KibanaStatus>()
                    .await
                    .ok()
                    .and_then(|s| s.version)
                    .and_then(|v| v.number)
                    .unwrap_or_else(|| "7.0.0".to_string());

                // Kibana 版本近似对应 ES 版本
                let es_ver = parse_es_major_version(&ver_str).unwrap_or(7);
                tracing::info!("Kibana 代理模式连接成功，Kibana 版本: {}", ver_str);
                Ok(Self {
                    config,
                    client,
                    es_major_version: es_ver,
                    mode: ConnMode::KibanaProxy,
                })
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Err(anyhow!(
                    "无法连接到 ELK（直连 ES 和 Kibana 代理均失败）\n\
                     Kibana 返回 {}: {}\n\
                     请检查：\n\
                     1. 地址是否正确（ES 直连如 http://host:9200，或 Kibana 如 https://host/kibana）\n\
                     2. 用户名/密码是否正确\n\
                     3. 跳板机是否能访问该地址",
                    status, &body[..body.len().min(200)]
                ))
            }
            Err(e) => Err(anyhow!(
                "连接失败: {}\n请确认地址可达（ES 直连或 Kibana URL）",
                e
            )),
        }
    }

    fn build_trace_query(
        &self,
        trace_id: &str,
        service: Option<&str>,
        window: &TimeWindow,
    ) -> serde_json::Value {
        let mut must = vec![
            serde_json::json!({ "term": { self.config.field_mapping.trace_id.clone(): trace_id } }),
        ];
        // 只在 window 非空时才加 range filter，避免空字符串导致 ES 400
        if !window.start.is_empty() && !window.end.is_empty() {
            must.push(serde_json::json!({ "range": {
                self.config.field_mapping.timestamp.clone(): {
                    "gte": window.start, "lte": window.end
                }
            }}));
        }
        if let Some(svc) = service {
            must.push(
                serde_json::json!({ "term": { self.config.field_mapping.service.clone(): svc } }),
            );
        }
        serde_json::json!({
            "query": { "bool": { "must": must } },
            "size": self.config.max_hits_per_trace,
            "sort": [{ self.config.field_mapping.timestamp.clone(): "asc" }]
        })
    }

    fn build_keyword_query(
        &self,
        query_str: &str,
        service: Option<&str>,
        window: &TimeWindow,
    ) -> serde_json::Value {
        let mut must: Vec<serde_json::Value> = vec![
            // query_string 对含特殊字符（点号、冒号）的 traceId 会解析出错，
            // 用 escaped 形式或改为 multi_match 以保证兼容性
            serde_json::json!({
                "query_string": {
                    "query": query_str,
                    "analyze_wildcard": true,
                    // 关闭字段自动解析，当作 all-field 全文搜索
                    "default_operator": "AND"
                }
            }),
            serde_json::json!({ "range": {
                self.config.field_mapping.timestamp.clone(): {
                    "gte": window.start, "lte": window.end
                }
            }}),
        ];
        if let Some(svc) = service {
            must.push(
                serde_json::json!({ "term": { self.config.field_mapping.service.clone(): svc } }),
            );
        }
        serde_json::json!({
            "query": { "bool": { "must": must } },
            "size": 5000,
            "sort": [{ self.config.field_mapping.timestamp.clone(): "asc" }]
        })
    }

    /// 按单个 traceId 精确查询（用 term，避免 query_string 对点号的解析问题）
    fn build_exact_trace_query(&self, trace_id: &str, window: &TimeWindow) -> serde_json::Value {
        let mut must = vec![
            serde_json::json!({ "term": { self.config.field_mapping.trace_id.clone(): trace_id } }),
        ];
        if !window.start.is_empty() && !window.end.is_empty() {
            must.push(serde_json::json!({ "range": {
                self.config.field_mapping.timestamp.clone(): {
                    "gte": window.start, "lte": window.end
                }
            }}));
        }
        serde_json::json!({
            "query": { "bool": { "must": must } },
            "size": self.config.max_hits_per_trace,
            "sort": [{ self.config.field_mapping.timestamp.clone(): "asc" }]
        })
    }

    fn build_exact_trace_queries(
        &self,
        trace_ids: &[String],
        window: &TimeWindow,
    ) -> Vec<serde_json::Value> {
        trace_ids
            .iter()
            .map(|trace_id| self.build_exact_trace_query(trace_id, window))
            .collect()
    }

    /// 按精确 traceId 列表逐个查询，确保 max_hits_per_trace 对每个 traceId 生效
    pub async fn query_by_exact_trace_ids(
        &self,
        trace_ids: &[String],
        window: &TimeWindow,
    ) -> Result<Vec<LogEntry>> {
        if trace_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut merged = Vec::new();
        for body in self.build_exact_trace_queries(trace_ids, window) {
            merged.extend(self.execute_search(&body).await?);
        }
        merged.sort_by(|left, right| left.time.cmp(&right.time));
        Ok(merged)
    }

    /// 构建 level 过滤查询（level:(ERROR OR WARN)）
    fn build_level_filter_query(
        &self,
        levels: &[String],
        service: Option<&str>,
        window: &TimeWindow,
        extra_keywords: &[String],
    ) -> serde_json::Value {
        let level_query = if levels.len() == 1 {
            serde_json::json!({ "term": { self.config.field_mapping.level.clone(): levels[0] } })
        } else {
            let level_terms: Vec<serde_json::Value> = levels.iter()
                .map(|l| serde_json::json!({ "term": { self.config.field_mapping.level.clone(): l } }))
                .collect();
            serde_json::json!({ "bool": { "should": level_terms, "minimum_should_match": 1 } })
        };

        let mut must = vec![
            level_query,
            serde_json::json!({ "range": { self.config.field_mapping.timestamp.clone(): { "gte": window.start, "lte": window.end } } }),
        ];
        if let Some(svc) = service {
            must.push(
                serde_json::json!({ "term": { self.config.field_mapping.service.clone(): svc } }),
            );
        }
        for kw in extra_keywords {
            must.push(serde_json::json!({ "query_string": { "query": kw } }));
        }

        serde_json::json!({
            "query": { "bool": { "must": must } },
            "size": self.config.max_hits_per_trace,
            "sort": [{ self.config.field_mapping.timestamp.clone(): "asc" }]
        })
    }

    /// 构建搜索 URL（根据连接模式）
    fn search_url(&self) -> String {
        let base = self.config.address.trim_end_matches('/');
        match self.mode {
            // 直连 ES：http://host:9200/index/_search
            ConnMode::DirectEs => format!("{}/{}/_search", base, self.config.index_pattern),

            // Kibana Console 代理（适用于 Kibana 7.x / 8.x）
            // 路径：/api/console/proxy?path=%2F{index}%2F_search&method=POST
            // 需要：kbn-xsrf: true  +  Basic Auth
            ConnMode::KibanaProxy => {
                // 对 ES 路径做 URL 编码：/pcm-java-log/_search → %2Fpcm-java-log%2F_search
                let es_path = format!("/{}/_search", self.config.index_pattern)
                    .replace('/', "%2F")
                    .replace('*', "%2A");
                format!("{}/api/console/proxy?path={}&method=POST", base, es_path)
            }
        }
    }

    async fn execute_search(&self, body: &serde_json::Value) -> Result<Vec<LogEntry>> {
        let url = self.search_url();

        tracing::info!("ELK execute_search URL: {}", url);
        tracing::info!(
            "ELK execute_search body: {}",
            serde_json::to_string(body).unwrap_or_default()
        );

        let mut req = self.client.post(&url).json(body);
        if let (Some(u), Some(p)) = (&self.config.username, &self.config.password) {
            req = req.basic_auth(u, Some(p));
        }
        // Kibana Console 代理模式需要 kbn-xsrf header
        if self.mode == ConnMode::KibanaProxy {
            req = req.header("kbn-xsrf", "true");
        }

        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("ELK 查询失败: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let hint = if status.as_u16() == 404 && self.mode == ConnMode::KibanaProxy {
                "\n提示：请确认索引名称正确（如 pcm-java-log），并确认该 Kibana 用户有 Discover 权限"
            } else {
                ""
            };
            return Err(anyhow!("ELK 返回错误 {}: {}{}", status, text, hint));
        }

        let resp_text = resp
            .text()
            .await
            .map_err(|e| anyhow!("ELK 响应读取失败: {}", e))?;

        let result: serde_json::Value = serde_json::from_str(&resp_text)
            .map_err(|e| anyhow!("ELK 响应 JSON 解析失败: {}", e))?;

        // Kibana Console 代理可能返回 HTTP 200 但 body 内含 ES 错误
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
            let keys: Vec<&str> = result
                .as_object()
                .map(|o| o.keys().map(|k| k.as_str()).collect())
                .unwrap_or_default();
            anyhow!("ELK 响应格式异常（顶层 keys={:?}）", keys)
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
                    service: source
                        .get(&self.config.field_mapping.service)
                        .and_then(|s| s.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    trace_id: source
                        .get(&self.config.field_mapping.trace_id)
                        .and_then(|t| t.as_str())
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
                    message: source
                        .get(&self.config.field_mapping.message)
                        .and_then(|m| m.as_str())
                        .unwrap_or("")
                        .to_string(),
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

    /// 返回 ES 主版本号（供连通性测试使用）
    pub fn es_major_version(&self) -> u8 {
        self.es_major_version
    }

    async fn parse_search_response(
        resp: reqwest::Response,
        field_mapping: &diag_core::config::FieldMapping,
    ) -> Result<Vec<LogEntry>> {
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("ELK 返回错误 {}: {}", status, text));
        }

        let resp_text = resp
            .text()
            .await
            .map_err(|e| anyhow!("ELK 响应读取失败: {}", e))?;

        let result: serde_json::Value = serde_json::from_str(&resp_text).map_err(|e| {
            anyhow!(
                "ELK 响应 JSON 解析失败: {} | body={}",
                e,
                &resp_text[..resp_text.len().min(300)]
            )
        })?;

        // Kibana Console 代理可能返回 HTTP 200 但 body 内含 ES 错误
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
            let keys: Vec<&str> = result
                .as_object()
                .map(|o| o.keys().map(|k| k.as_str()).collect())
                .unwrap_or_default();
            anyhow!(
                "ELK 响应格式异常（顶层 keys={:?}），body={}",
                keys,
                &resp_text[..resp_text.len().min(300)]
            )
        })?;

        let entries: Vec<LogEntry> = hits
            .iter()
            .filter_map(|hit| {
                let source = hit.get("_source")?;
                Some(LogEntry {
                    time: source
                        .get(&field_mapping.timestamp)
                        .and_then(|t| t.as_str())
                        .map(String::from),
                    level: source
                        .get(&field_mapping.level)
                        .and_then(|l| l.as_str())
                        .unwrap_or("UNKNOWN")
                        .to_string(),
                    service: source
                        .get(&field_mapping.service)
                        .and_then(|s| s.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    trace_id: source
                        .get(&field_mapping.trace_id)
                        .and_then(|t| t.as_str())
                        .map(String::from),
                    thread: source
                        .get(&field_mapping.thread)
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
                    message: source
                        .get(&field_mapping.message)
                        .and_then(|m| m.as_str())
                        .unwrap_or("")
                        .to_string(),
                    exception: source
                        .get(&field_mapping.exception)
                        .and_then(|e| e.as_str())
                        .map(String::from),
                    stack_trace: source
                        .get(&field_mapping.stack_trace)
                        .and_then(|s| s.as_str())
                        .map(String::from),
                    raw: serde_json::to_string(source).unwrap_or_default(),
                })
            })
            .collect();

        Ok(entries)
    }

    /// 按 level 列表查询日志（OR 逻辑），供调度器调用
    pub async fn query_by_levels(
        &self,
        levels: &[String],
        service: Option<&str>,
        window: &TimeWindow,
        extra_keywords: &[String],
    ) -> Result<Vec<LogEntry>> {
        let body = self.build_level_filter_query(levels, service, window, extra_keywords);
        self.execute_search(&body).await
    }
}

#[async_trait]
impl LogCollector for ElkCollector {
    async fn query_by_trace_ids(
        &self,
        trace_ids: &[String],
        _service: Option<&str>,
        window: &TimeWindow,
    ) -> Result<Vec<LogEntry>> {
        // 使用逐个 traceId 精确查询，避免 terms 查询的全局 size 截断掉部分 traceId 日志
        self.query_by_exact_trace_ids(trace_ids, window).await
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
