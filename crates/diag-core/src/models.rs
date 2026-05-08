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
