use regex::Regex;
use std::sync::LazyLock;

static TABLE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:FROM|JOIN|INTO|UPDATE|TABLE)\s+`?(\w+)`?").unwrap()
});

static PARAM_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"'[^']*'").unwrap()
});

static NUMBER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b\d+\b").unwrap()
});

static WHITESPACE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s+").unwrap()
});

/// 生成 SQL 指纹（脱敏参数值）
pub fn fingerprint_sql(sql: &str) -> String {
    let result = PARAM_REGEX.replace_all(sql, "?");
    let result = NUMBER_REGEX.replace_all(&result, "?");
    let result = WHITESPACE_REGEX.replace_all(&result, " ");
    result.trim().to_string()
}

/// 从 SQL 中提取涉及的表名
pub fn extract_tables(sql: &str) -> Vec<String> {
    TABLE_REGEX
        .captures_iter(sql)
        .filter_map(|cap| cap.get(1))
        .map(|m| m.as_str().to_lowercase())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect()
}

/// 判断 SQL 操作类型
pub fn detect_operation(sql: &str) -> &'static str {
    let trimmed = sql.trim_start().to_uppercase();
    if trimmed.starts_with("SELECT") {
        "SELECT"
    } else if trimmed.starts_with("INSERT") {
        "INSERT"
    } else if trimmed.starts_with("UPDATE") {
        "UPDATE"
    } else if trimmed.starts_with("DELETE") {
        "DELETE"
    } else {
        "OTHER"
    }
}

/// MySQL 慢查询日志解析
pub fn parse_mysql_slow_log_entry(block: &str) -> Option<(String, f64, i64, i64)> {
    let mut query_time: Option<f64> = None;
    let mut rows_examined: i64 = 0;
    let mut rows_sent: i64 = 0;
    let mut sql = String::new();

    for line in block.lines() {
        if line.starts_with("# Query_time:") {
            // # Query_time: 1.530000  Lock_time: 0.000100 Rows_sent: 10  Rows_examined: 200000
            let parts: Vec<&str> = line.split_whitespace().collect();
            for (i, part) in parts.iter().enumerate() {
                match *part {
                    "Query_time:" => {
                        query_time = parts.get(i + 1).and_then(|v| v.parse().ok());
                    }
                    "Rows_sent:" => {
                        rows_sent = parts.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(0);
                    }
                    "Rows_examined:" => {
                        rows_examined = parts.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(0);
                    }
                    _ => {}
                }
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            if !sql.is_empty() {
                sql.push(' ');
            }
            sql.push_str(line.trim_end_matches(';'));
        }
    }

    query_time.map(|qt| (sql, qt * 1000.0, rows_examined, rows_sent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fingerprint_sql() {
        let sql = "SELECT * FROM speech_module WHERE disease_name = '糖尿病' AND page_num = 1 LIMIT 10";
        let fp = fingerprint_sql(sql);
        assert_eq!(fp, "SELECT * FROM speech_module WHERE disease_name = ? AND page_num = ? LIMIT ?");
    }

    #[test]
    fn test_extract_tables() {
        let sql = "SELECT a.* FROM speech_module a JOIN disease_type b ON a.disease_id = b.id";
        let tables = extract_tables(sql);
        assert!(tables.contains(&"speech_module".to_string()));
        assert!(tables.contains(&"disease_type".to_string()));
    }

    #[test]
    fn test_detect_operation() {
        assert_eq!(detect_operation("SELECT * FROM t"), "SELECT");
        assert_eq!(detect_operation("  update t set a=1"), "UPDATE");
        assert_eq!(detect_operation("INSERT INTO t VALUES (1)"), "INSERT");
        assert_eq!(detect_operation("DELETE FROM t WHERE id=1"), "DELETE");
    }
}
