use diag_core::config::CollectorConfig;
use diag_core::models::{CapturedPage, ResolvedUrl};
use diag_core::url_resolver;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::State;

use crate::db_collector::DbCollector;
use crate::diagnosis::DiagnosisRunner;
use crate::ssh_collector;

/// 全局应用状态
pub struct AppState {
    pub config: Mutex<Option<CollectorConfig>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosisStatus {
    pub phase: String,
    pub progress: f32,
    pub message: String,
    pub completed: bool,
    pub output_path: Option<String>,
    pub error: Option<String>,
}

// ─── 配置管理 ───

/// 加载配置文件
#[tauri::command]
pub fn load_config(config_path: String, state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let config = CollectorConfig::load(&config_path).map_err(|e| format!("配置加载失败: {}", e))?;

    let info = serde_json::json!({
        "site": config.site.name,
        "system": config.site.system,
        "serviceCount": config.services.len(),
        "services": config.services.iter().map(|s| {
            serde_json::json!({
                "name": s.name,
                "display": s.display,
                "hosts": s.hosts,
                "logDir": s.log_dir,
            })
        }).collect::<Vec<_>>(),
        "sshUser": config.ssh.username,
        "sshAuthType": config.ssh.auth_type,
        "dbType": config.database.db_type,
        "dbHost": config.database.host,
    });

    *state.config.lock().unwrap() = Some(config);
    Ok(info)
}

// ─── URL 解析 ───

/// 解析单个 Request URL
#[tauri::command]
pub fn resolve_request_url(url: String, gateway_prefix: String) -> Result<ResolvedUrl, String> {
    url_resolver::resolve_url(&url, &gateway_prefix).map_err(|e| e.to_string())
}

/// 批量解析 URL 并按服务分组
#[tauri::command]
pub fn resolve_batch_urls(
    urls: Vec<String>,
    gateway_prefix: String,
) -> Result<serde_json::Value, String> {
    let mut service_map: std::collections::HashMap<String, Vec<serde_json::Value>> =
        std::collections::HashMap::new();

    for url in &urls {
        if let Ok(resolved) = url_resolver::resolve_url(url, &gateway_prefix) {
            let entry = serde_json::json!({
                "url": url,
                "apiPath": resolved.api_path,
                "resource": resolved.resource,
                "operation": resolved.operation,
            });
            service_map
                .entry(resolved.service.clone())
                .or_default()
                .push(entry);
        }
    }

    Ok(serde_json::json!({
        "totalUrls": urls.len(),
        "serviceCount": service_map.len(),
        "services": service_map,
    }))
}

// ─── SSH 连接测试 ───

/// 测试 SSH 连接
#[tauri::command]
pub async fn test_ssh_connection(config_path: String, host: String) -> Result<String, String> {
    let config = CollectorConfig::load(&config_path).map_err(|e| format!("配置加载失败: {}", e))?;

    match ssh_collector::ssh_exec(&host, &config.ssh, "echo 'SSH_OK' && hostname && date").await {
        Ok(output) => Ok(format!("SSH 连接成功\n{}", output.trim())),
        Err(e) => Err(format!("SSH 连接失败: {}", e)),
    }
}

/// 列出远程日志文件
#[tauri::command]
pub async fn list_remote_log_files(
    config_path: String,
    service_name: String,
) -> Result<Vec<String>, String> {
    let config = CollectorConfig::load(&config_path).map_err(|e| format!("配置加载失败: {}", e))?;

    let svc = config
        .find_service(&service_name)
        .ok_or_else(|| format!("未找到服务配置: {}", service_name))?;

    let mut all_files = Vec::new();
    for host in &svc.hosts {
        match ssh_collector::list_remote_logs(host, &config.ssh, &svc.log_dir, &svc.log_pattern)
            .await
        {
            Ok(files) => {
                for f in files {
                    all_files.push(format!("[{}] {}", host, f));
                }
            }
            Err(e) => {
                all_files.push(format!("[{}] 查询失败: {}", host, e));
            }
        }
    }

    Ok(all_files)
}

// ─── 数据库测试 ───

/// 测试数据库连接
#[tauri::command]
pub async fn test_db_connection(config_path: String) -> Result<String, String> {
    let config = CollectorConfig::load(&config_path).map_err(|e| format!("配置加载失败: {}", e))?;

    let collector = DbCollector::new(config.database.clone());
    match collector.collect().await {
        Ok((sqls, stats)) => Ok(format!(
            "数据库连接成功\n慢 SQL: {} 条\n表统计: {} 张表",
            sqls.len(),
            stats.len()
        )),
        Err(e) => Err(format!("数据库连接失败: {}", e)),
    }
}

// ─── 诊断流程 ───

/// 启动完整诊断流程（接收前端 WebView 捕获的请求数据）
#[tauri::command]
pub async fn start_diagnosis(captured_json: String, config_path: String) -> Result<String, String> {
    // 解析前端传来的捕获数据
    let captured: CapturedPage =
        serde_json::from_str(&captured_json).map_err(|e| format!("解析捕获数据失败: {}", e))?;

    // 加载配置
    let config = CollectorConfig::load(&config_path).map_err(|e| format!("加载配置失败: {}", e))?;

    tracing::info!(
        "启动诊断: 页面={}, 请求数={}, 站点={}",
        captured.page_url,
        captured.requests.len(),
        config.site.name
    );

    // 运行诊断流程
    let runner = DiagnosisRunner::new(config, captured);
    match runner.run().await {
        Ok(output_path) => {
            tracing::info!("诊断完成: {}", output_path);
            Ok(output_path)
        }
        Err(e) => {
            tracing::error!("诊断执行失败: {}", e);
            Err(format!("诊断执行失败: {}", e))
        }
    }
}

/// 获取诊断状态
#[tauri::command]
pub fn get_diagnosis_status() -> DiagnosisStatus {
    DiagnosisStatus::default()
}
