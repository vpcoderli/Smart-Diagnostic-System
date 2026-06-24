use crate::models::ResolvedUrl;
use anyhow::{anyhow, Result};

/// 从 Request URL 解析出服务名和 API 路径
///
/// 输入: http://172.29.60.151/gateway/pcm-management/v1/pt/speech-module/list?pageNum=1
/// 输出: ResolvedUrl { service: "pcm-management", api_path: "/v1/pt/speech-module/list", ... }
pub fn resolve_url(raw_url: &str, gateway_prefix: &str) -> Result<ResolvedUrl> {
    // 去掉协议和域名，提取路径
    let url = url::Url::parse(raw_url)
        .map_err(|e| anyhow!("无法解析 URL '{}': {}", raw_url, e))?;

    let host = url.host_str().unwrap_or("unknown").to_string();
    let path = url.path();

    // 去掉 gateway prefix
    let prefix = gateway_prefix.trim_end_matches('/');
    let after_gateway = if path.starts_with(prefix) {
        &path[prefix.len()..]
    } else {
        path
    };

    // 提取 service 和 api_path
    // /pcm-management/v1/pt/speech-module/list
    let parts: Vec<&str> = after_gateway
        .trim_start_matches('/')
        .splitn(2, '/')
        .collect();

    if parts.is_empty() || parts[0].is_empty() {
        return Err(anyhow!("无法从路径 '{}' 中识别服务名", path));
    }

    let service = parts[0].to_string();
    let api_path = if parts.len() > 1 {
        format!("/{}", parts[1])
    } else {
        "/".to_string()
    };

    // 从 api_path 提取 resource 和 operation
    let path_segments: Vec<&str> = api_path
        .trim_end_matches('/')
        .rsplit('/')
        .collect();

    let operation = path_segments.first().map(|s| s.to_string());
    let resource = path_segments.get(1).map(|s| s.to_string());

    Ok(ResolvedUrl {
        host,
        gateway_prefix: prefix.to_string(),
        service,
        api_path,
        resource,
        operation,
    })
}

/// 已知的 pcm 服务列表
pub const KNOWN_SERVICES: &[(&str, &str)] = &[
    ("pcm-server", "患者管理服务"),
    ("pcm-followup", "随访服务"),
    ("pcm-communication", "会话服务"),
    ("pcm-management", "业务管理服务"),
    ("pcm-profile", "画像服务"),
    ("pcm-data", "数据服务"),
    ("pcm-statistics", "数据分析服务"),
    ("pcm-user", "用户服务"),
    ("pcm-channel", "通道服务"),
    ("pcm-health-plan", "健康方案服务"),
    ("pcm-open-api", "外部接口服务"),
];

/// 判断是否为已知的 pcm 服务
pub fn is_known_service(service_name: &str) -> bool {
    KNOWN_SERVICES.iter().any(|(name, _)| *name == service_name)
}

/// 获取服务显示名称
pub fn service_display_name(service_name: &str) -> &str {
    KNOWN_SERVICES
        .iter()
        .find(|(name, _)| *name == service_name)
        .map(|(_, display)| *display)
        .unwrap_or(service_name)
}
