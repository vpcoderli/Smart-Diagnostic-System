use crate::models::LogEntry;
use regex::Regex;
use std::sync::LazyLock;

static JSON_LOG_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\{.*"traceId".*\}"#).unwrap()
});

static TEXT_LOG_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?P<time>\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}[.\d]*[+\-\d:Z]*)\s+(?P<level>\w+)\s+.*"
    ).unwrap()
});

static EXCEPTION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<exception>[a-zA-Z.]+Exception):\s*(?P<message>.*)").unwrap()
});

/// 解析日志行（支持 JSON 和文本两种格式）
pub fn parse_log_line(line: &str, service_name: &str) -> LogEntry {
    // 先尝试 JSON 解析
    if let Ok(entry) = try_parse_json_log(line, service_name) {
        return entry;
    }
    // 退回文本解析
    parse_text_log(line, service_name)
}

fn try_parse_json_log(line: &str, service_name: &str) -> anyhow::Result<LogEntry> {
    let v: serde_json::Value = serde_json::from_str(line)?;
    Ok(LogEntry {
        time: v.get("time").or(v.get("timestamp")).and_then(|t| t.as_str()).map(String::from),
        level: v.get("level").and_then(|l| l.as_str()).unwrap_or("UNKNOWN").to_string(),
        service: v.get("service").and_then(|s| s.as_str()).unwrap_or(service_name).to_string(),
        trace_id: v.get("traceId").and_then(|t| t.as_str()).map(String::from),
        thread: v.get("thread").and_then(|t| t.as_str()).map(String::from),
        class: v.get("class").and_then(|c| c.as_str()).map(String::from),
        method: v.get("method").and_then(|m| m.as_str()).map(String::from),
        message: v.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string(),
        exception: v.get("exception").and_then(|e| e.as_str()).map(String::from),
        stack_trace: v.get("stack").or(v.get("stackTrace")).and_then(|s| s.as_str()).map(String::from),
        raw: line.to_string(),
    })
}

fn parse_text_log(line: &str, service_name: &str) -> LogEntry {
    let mut entry = LogEntry {
        time: None,
        level: "UNKNOWN".to_string(),
        service: service_name.to_string(),
        trace_id: None,
        thread: None,
        class: None,
        method: None,
        message: line.to_string(),
        exception: None,
        stack_trace: None,
        raw: line.to_string(),
    };

    // 尝试提取时间和级别
    if let Some(caps) = TEXT_LOG_REGEX.captures(line) {
        if let Some(time) = caps.name("time") {
            entry.time = Some(time.as_str().to_string());
        }
        if let Some(level) = caps.name("level") {
            entry.level = level.as_str().to_uppercase();
        }
    }

    // 尝试提取异常类
    if let Some(caps) = EXCEPTION_REGEX.captures(line) {
        if let Some(exc) = caps.name("exception") {
            entry.exception = Some(exc.as_str().to_string());
        }
        if let Some(msg) = caps.name("message") {
            entry.message = msg.as_str().to_string();
        }
    }

    entry
}

/// 从日志行中提取 traceId
pub fn extract_trace_id(line: &str) -> Option<String> {
    // JSON 格式: "traceId":"abc123"
    if let Some(start) = line.find("\"traceId\":\"") {
        let rest = &line[start + 11..];
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    // MDC 格式: [traceId=abc123]
    if let Some(start) = line.find("traceId=") {
        let rest = &line[start + 8..];
        let end = rest.find(|c: char| !c.is_alphanumeric() && c != '-').unwrap_or(rest.len());
        if end > 0 {
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// 判断日志行是否为 ERROR 级别
pub fn is_error_log(line: &str) -> bool {
    line.contains("\"level\":\"ERROR\"")
        || line.contains(" ERROR ")
        || line.contains("[ERROR]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_json_log() {
        let line = r#"{"time":"2026-05-08T10:00:01.123+08:00","level":"ERROR","service":"pcm-management","traceId":"abc123","message":"Query failed","exception":"java.sql.SQLTimeoutException"}"#;
        let entry = parse_log_line(line, "pcm-management");
        assert_eq!(entry.level, "ERROR");
        assert_eq!(entry.trace_id.as_deref(), Some("abc123"));
        assert_eq!(entry.exception.as_deref(), Some("java.sql.SQLTimeoutException"));
    }

    #[test]
    fn test_extract_trace_id_json() {
        let line = r#"{"traceId":"abc123","level":"INFO"}"#;
        assert_eq!(extract_trace_id(line), Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_trace_id_mdc() {
        let line = "2026-05-08 10:00:01 INFO [traceId=def456] SomeClass - message";
        assert_eq!(extract_trace_id(line), Some("def456".to_string()));
    }

    #[test]
    fn test_is_error_log() {
        assert!(is_error_log(r#"{"level":"ERROR","message":"fail"}"#));
        assert!(is_error_log("2026-05-08 10:00:01 ERROR SomeClass - fail"));
        assert!(!is_error_log("2026-05-08 10:00:01 INFO SomeClass - ok"));
    }
}
