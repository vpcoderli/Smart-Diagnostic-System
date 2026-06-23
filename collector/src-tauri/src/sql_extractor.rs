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
    fn test_extract_sql_traces_with_parameters() {
        let logs = vec![
            LogEntry {
                time: Some("2026-05-26T10:00:00".into()),
                level: "DEBUG".into(),
                service: "pcm-management".into(),
                trace_id: Some("abc123".into()),
                thread: Some("http-nio-8080-exec-1".into()),
                class: None,
                method: None,
                message: "==>  Preparing: SELECT * FROM patient WHERE id = ? AND status = ?".into(),
                exception: None,
                stack_trace: None,
                raw: "".into(),
            },
            LogEntry {
                time: Some("2026-05-26T10:00:00".into()),
                level: "DEBUG".into(),
                service: "pcm-management".into(),
                trace_id: Some("abc123".into()),
                thread: Some("http-nio-8080-exec-1".into()),
                class: None,
                method: None,
                message: "==> Parameters: 1001(Long), 1(Integer)".into(),
                exception: None,
                stack_trace: None,
                raw: "".into(),
            },
        ];
        let traces = extract_sql_traces(&logs);
        assert_eq!(traces.len(), 1);
        assert_eq!(
            traces[0].parameters.as_deref(),
            Some("1001(Long), 1(Integer)")
        );
    }

    #[test]
    fn test_extract_sql_traces_from_logs() {
        let logs = vec![LogEntry {
            time: Some("2026-05-26T10:00:00".into()),
            level: "DEBUG".into(),
            service: "pcm-management".into(),
            trace_id: Some("abc123".into()),
            thread: None,
            class: None,
            method: None,
            message: "==>  Preparing: SELECT * FROM patient WHERE id = ?".into(),
            exception: None,
            stack_trace: None,
            raw: "".into(),
        }];
        let traces = extract_sql_traces(&logs);
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].trace_id, "abc123");
        assert_eq!(traces[0].service, "pcm-management");
    }

    #[test]
    fn test_skip_log_without_trace_id() {
        let logs = vec![LogEntry {
            time: None,
            level: "DEBUG".into(),
            service: "pcm-management".into(),
            trace_id: None,
            thread: None,
            class: None,
            method: None,
            message: "==>  Preparing: SELECT * FROM patient WHERE id = ?".into(),
            exception: None,
            stack_trace: None,
            raw: "".into(),
        }];
        let traces = extract_sql_traces(&logs);
        assert_eq!(traces.len(), 0);
    }
}
