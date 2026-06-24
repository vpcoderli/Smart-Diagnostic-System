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

/// 解析 MyBatis Parameters 行字符串为 (值, 类型) 列表
/// 输入示例: "218713736305705076(String), 1(Integer), null"
fn parse_mybatis_parameters(params_str: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let mut buf = String::new();
    let mut depth = 0;
    let mut in_quote = false;

    for ch in params_str.chars() {
        match ch {
            '(' if !in_quote => { depth += 1; buf.push(ch); }
            ')' if !in_quote => { depth -= 1; buf.push(ch); }
            '\'' => { in_quote = !in_quote; buf.push(ch); }
            ',' if depth == 0 && !in_quote => {
                push_parameter(&mut result, buf.trim());
                buf.clear();
            }
            _ => buf.push(ch),
        }
    }
    if !buf.trim().is_empty() {
        push_parameter(&mut result, buf.trim());
    }
    result
}

fn push_parameter(result: &mut Vec<(String, String)>, raw: &str) {
    let raw = raw.trim();
    if raw.is_empty() { return; }
    if raw.eq_ignore_ascii_case("null") {
        result.push(("null".to_string(), "null".to_string()));
        return;
    }
    if let Some(open) = raw.rfind('(') {
        if raw.ends_with(')') {
            let value = raw[..open].trim().to_string();
            let type_name = raw[open + 1..raw.len() - 1].trim().to_string();
            result.push((value, type_name));
            return;
        }
    }
    result.push((raw.to_string(), String::new()));
}

fn needs_quote(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    !matches!(
        t.as_str(),
        "integer" | "int" | "long" | "short" | "byte"
        | "float" | "double" | "decimal" | "bigdecimal" | "biginteger"
        | "boolean" | "bool" | "null"
    )
}

fn escape_sql_value(v: &str) -> String {
    v.replace('\'', "''")
}

/// 将 SQL 中的 `?` 占位符按 MyBatis Parameters 行的值顺序替换。
/// 字符串/日期等类型加单引号，数值不加，null 替换为 NULL；字符串字面量内的 `?` 不替换。
pub fn substitute_mybatis_parameters(sql: &str, params_str: &str) -> String {
    let params = parse_mybatis_parameters(params_str);
    if params.is_empty() {
        return sql.to_string();
    }

    let mut out = String::with_capacity(sql.len() + 32);
    let mut idx = 0;
    let mut in_quote = false;
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\'' {
            if in_quote && i + 1 < chars.len() && chars[i + 1] == '\'' {
                out.push('\'');
                out.push('\'');
                i += 2;
                continue;
            }
            in_quote = !in_quote;
            out.push(ch);
            i += 1;
            continue;
        }
        if ch == '?' && !in_quote {
            if let Some((value, type_name)) = params.get(idx) {
                if value.eq_ignore_ascii_case("null") {
                    out.push_str("NULL");
                } else if needs_quote(type_name) {
                    out.push('\'');
                    out.push_str(&escape_sql_value(value));
                    out.push('\'');
                } else {
                    out.push_str(value);
                }
                idx += 1;
            } else {
                out.push('?');
            }
            i += 1;
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
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

pub fn has_unresolved_placeholder(sql: &str) -> bool {
    let mut in_quote = false;
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\'' {
            if in_quote && i + 1 < chars.len() && chars[i + 1] == '\'' {
                i += 2;
                continue;
            }
            in_quote = !in_quote;
        } else if ch == '?' && !in_quote {
            return true;
        }
        i += 1;
    }
    false
}

pub fn substitute_dummy_parameters(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 32);
    let mut in_quote = false;
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\'' {
            if in_quote && i + 1 < chars.len() && chars[i + 1] == '\'' {
                out.push('\'');
                out.push('\'');
                i += 2;
                continue;
            }
            in_quote = !in_quote;
            out.push(ch);
            i += 1;
            continue;
        }
        
        if ch == '?' && !in_quote {
            let is_limit_or_offset = is_limit_or_offset_context(&chars, i);
            if is_limit_or_offset {
                out.push_str("1");
            } else {
                out.push_str("'1'");
            }
            i += 1;
            continue;
        }
        
        out.push(ch);
        i += 1;
    }
    out
}

fn is_limit_or_offset_context(chars: &[char], index: usize) -> bool {
    if index == 0 {
        return false;
    }
    let mut pos = index - 1;
    
    while pos > 0 {
        let ch = chars[pos];
        if ch.is_whitespace() || ch == ',' || ch.is_ascii_digit() || ch == '?' {
            pos -= 1;
        } else {
            break;
        }
    }
    
    let mut word_chars = Vec::new();
    while pos > 0 {
        let ch = chars[pos];
        if ch.is_alphabetic() {
            word_chars.push(ch.to_ascii_uppercase());
            if pos == 0 {
                break;
            }
            pos -= 1;
        } else {
            if !ch.is_whitespace() && ch != ',' && ch != '?' && !ch.is_ascii_digit() {
                break;
            }
            break;
        }
    }
    
    if pos == 0 && chars[0].is_alphabetic() {
        word_chars.push(chars[0].to_ascii_uppercase());
    }
    
    word_chars.reverse();
    let word: String = word_chars.into_iter().collect();
    word == "LIMIT" || word == "OFFSET"
}

