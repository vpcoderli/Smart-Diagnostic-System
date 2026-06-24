use diag_core::models::{LogEntry, SqlTrace};
use diag_core::sql_parser;
use regex::Regex;
use std::sync::LazyLock;

static MYBATIS_SQL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)==>\s+Preparing:\s+(.+)$").unwrap());

static MYBATIS_PARAMS_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)==>\s+Parameters:\s+(.+)$").unwrap());

static HIBERNATE_SQL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^Hibernate:\s+(.+)$").unwrap());

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
    if let Some(mat) = GENERIC_SQL_REGEX.find(line) {
        return Some(mat.as_str().trim().to_string());
    }
    None
}

fn extract_params_from_line(line: &str) -> Option<String> {
    MYBATIS_PARAMS_REGEX
        .captures(line)
        .map(|cap| cap[1].trim().to_string())
}

/// Re-export for callers within the collector binary.
pub use diag_core::sql_parser::substitute_mybatis_parameters as substitute_parameters;

pub fn extract_sql_traces(logs: &[LogEntry]) -> Vec<SqlTrace> {
    let mut traces = Vec::new();

    let mut i = 0;
    while i < logs.len() {
        let entry = &logs[i];

        let trace_id = match &entry.trace_id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => {
                i += 1;
                continue;
            }
        };

        let sql = match extract_sql_from_line(&entry.message) {
            Some(s) => s,
            None => match extract_sql_from_line(&entry.raw) {
                Some(s) => s,
                None => {
                    i += 1;
                    continue;
                }
            },
        };

        // 尝试从当前行或紧随其后的同 service+thread 行提取 MyBatis Parameters
        let mut parameters: Option<String> = extract_params_from_line(&entry.message)
            .or_else(|| extract_params_from_line(&entry.raw));

        if parameters.is_none() {
            // 向后查找紧邻的 Parameters 行（同 service + thread）
            if i + 1 < logs.len() {
                let next = &logs[i + 1];
                let same_ctx = next.service == entry.service
                    && next.thread == entry.thread
                    && next.trace_id.as_deref() == entry.trace_id.as_deref();
                if same_ctx {
                    parameters = extract_params_from_line(&next.message)
                        .or_else(|| extract_params_from_line(&next.raw));
                    if parameters.is_some() {
                        i += 1; // 跳过已消费的 Parameters 行
                    }
                }
            }
        }

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
            parameters,
        });

        i += 1;
    }

    traces
}
