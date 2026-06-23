use serde::{Deserialize, Serialize};

// ─── 诊断包 Manifest ───

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
    pub collection_mode: Option<String>,   // "realtime" | "historical" | "scheduled"
    #[serde(default)]
    pub log_source: Option<String>,         // "elk" | "ssh"
    #[serde(default)]
    pub gateway_prefix: Option<String>,     // 供 analyzer 读取，不再硬编码 /gateway
    #[serde(default)]
    pub keywords: Option<Vec<String>>,
    #[serde(default)]
    pub time_range: Option<TimeWindow>,
}

// ─── 浏览器捕获数据 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedPage {
    pub page_url: String,
    pub requests: Vec<CapturedRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedRequest {
    pub method: String,
    pub url: String,
    pub status: u16,
    pub duration_ms: u64,
    pub trace_id: Option<String>,
    pub timestamp: String,
    #[serde(default)]
    pub request_type: String,
    #[serde(default)]
    pub response_size: Option<u64>,
}

// ─── URL 解析结果 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedUrl {
    pub host: String,
    pub gateway_prefix: String,
    pub service: String,
    pub api_path: String,
    pub resource: Option<String>,
    pub operation: Option<String>,
}

// ─── 日志条目 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub time: Option<String>,
    pub level: String,
    pub service: String,
    pub trace_id: Option<String>,
    pub thread: Option<String>,
    pub class: Option<String>,
    pub method: Option<String>,
    pub message: String,
    pub exception: Option<String>,
    pub stack_trace: Option<String>,
    pub raw: String,
}

// ─── 慢 SQL ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlowSqlItem {
    pub trace_id: Option<String>,
    pub database_type: String,
    pub service: Option<String>,
    pub sql_fingerprint: String,
    pub duration_ms: f64,
    pub tables: Vec<String>,
    pub operation: Option<String>,
    pub rows_examined: Option<i64>,
    pub rows_returned: Option<i64>,
    pub index_used: Option<bool>,
    pub explain_summary: Option<ExplainSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainSummary {
    pub access_type: Option<String>,
    pub possible_keys: Vec<String>,
    pub key_used: Option<String>,
    pub extra: Vec<String>,
}

// ─── 表统计 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableStats {
    pub schema: String,
    pub table_name: String,
    pub row_count: i64,
    pub data_size_bytes: Option<i64>,
    pub index_size_bytes: Option<i64>,
    pub indexes: Vec<IndexInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

// ─── 诊断结果 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub finding_type: FindingType,
    pub severity: Severity,
    pub summary: String,
    pub evidence: Vec<String>,
    pub short_term: Vec<String>,
    pub mid_term: Vec<String>,
    pub long_term: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FindingType {
    SlowSql,
    BackendException,
    SlowApi,
    MissingTrace,
    HttpError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

impl std::fmt::Display for FindingType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FindingType::SlowSql => write!(f, "慢SQL"),
            FindingType::BackendException => write!(f, "后端异常"),
            FindingType::SlowApi => write!(f, "慢接口"),
            FindingType::MissingTrace => write!(f, "缺失TraceId"),
            FindingType::HttpError => write!(f, "HTTP错误"),
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Critical => write!(f, "严重"),
            Severity::High => write!(f, "高"),
            Severity::Medium => write!(f, "中"),
            Severity::Low => write!(f, "低"),
            Severity::Info => write!(f, "信息"),
        }
    }
}

// ─── 诊断包完整数据模型 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosisPackage {
    pub manifest: DiagnosisManifest,
    pub captured_page: CapturedPage,
    pub logs: Vec<LogEntry>,
    pub slow_sqls: Vec<SlowSqlItem>,
    pub table_stats: Vec<TableStats>,
    pub collection_report: Option<CollectionReport>,
    #[serde(default)]
    pub sql_traces: Vec<SqlTrace>,
    #[serde(default)]
    pub explain_plans: Vec<ExplainPlan>,
}

// ─── 脱敏报告 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaskingReport {
    pub masked_query_params: Vec<String>,
    pub removed_headers: Vec<String>,
    pub masked_sql_params: bool,
    pub total_items_masked: usize,
}

// ─── v2: 采集时间窗口 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeWindow {
    pub start: String,
    pub end: String,
}

// ─── v2: 服务实例（来自 Nacos 等服务发现）───

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

// ─── v2: SQL Trace（从日志中提取的 SQL，按 traceId 关联）───

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
    #[serde(default)]
    pub parameters: Option<String>,
}

// ─── v2: EXPLAIN 执行计划 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainPlan {
    pub sql_fingerprint: String,
    pub avg_duration_ms: f64,
    pub source: String,
    pub explain_rows: Vec<ExplainRow>,
    pub table_stats: Option<TableStats>,
    /// 关联的日志 traceId（仅对来自日志 SQL 的 EXPLAIN 有值）
    #[serde(default)]
    pub trace_id: Option<String>,
    /// 拼装参数后实际执行的 SQL（仅对来自日志 SQL 的 EXPLAIN 有值）
    #[serde(default)]
    pub executed_sql: Option<String>,
    /// EXPLAIN 执行失败时的错误信息
    #[serde(default)]
    pub error: Option<String>,
    /// PostgreSQL 多 schema 场景：记录表实际所在的 schema
    #[serde(default)]
    pub found_in_schema: Option<String>,
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

// ─── 问题收集报告 ───

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CollectionReport {
    pub collected_at: String,
    pub log_source: String,
    pub log_count: usize,
    pub sql_trace_count: usize,
    pub explain_plan_count: usize,
    pub skipped_services: Vec<String>,
    pub errors: Vec<String>,
}
