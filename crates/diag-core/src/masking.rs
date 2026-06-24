use crate::config::PrivacyConfig;

/// 脱敏 URL 中的查询参数
pub fn mask_url(raw_url: &str, config: &PrivacyConfig) -> String {
    if !config.mask_query_values {
        return raw_url.to_string();
    }

    match url::Url::parse(raw_url) {
        Ok(mut url) => {
            let pairs: Vec<(String, String)> = url
                .query_pairs()
                .map(|(k, v)| {
                    let key = k.to_string();
                    if config.allowed_query_keys.contains(&key) {
                        (key, v.to_string())
                    } else if v.is_empty() {
                        (key, String::new())
                    } else {
                        (key, "***".to_string())
                    }
                })
                .collect();

            url.query_pairs_mut().clear();
            for (k, v) in &pairs {
                url.query_pairs_mut().append_pair(k, v);
            }
            url.to_string()
        }
        Err(_) => raw_url.to_string(),
    }
}

/// 需要过滤的 HTTP Header 名称
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "set-cookie",
    "token",
    "x-token",
    "x-auth-token",
];

/// 判断 header 是否需要过滤
pub fn is_sensitive_header(header_name: &str) -> bool {
    SENSITIVE_HEADERS.contains(&header_name.to_lowercase().as_str())
}

/// 需要脱敏的 URL 查询参数关键字
const SENSITIVE_QUERY_KEYS: &[&str] = &[
    "patientname",
    "phone",
    "idcard",
    "medicalrecord",
    "diagnosis",
    "password",
    "token",
];

/// 判断查询参数是否需要强制脱敏
pub fn is_sensitive_query_key(key: &str) -> bool {
    SENSITIVE_QUERY_KEYS.contains(&key.to_lowercase().as_str())
}
