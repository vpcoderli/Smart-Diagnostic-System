use diag_core::config::CollectorConfig;
use diag_core::models::{CapturedPage, CapturedRequest, ResolvedUrl};
use diag_core::url_resolver;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::State;

use crate::diagnosis::DiagnosisRunner;

/// 应用状态
pub struct AppState {
    pub config: Mutex<Option<CollectorConfig>>,
    pub diagnosis_status: Mutex<DiagnosisStatus>,
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

/// 加载配置文件
#[tauri::command]
pub fn load_config(config_path: String) -> Result<String, String> {
    match CollectorConfig::load(&config_path) {
        Ok(config) => {
            let site = config.site.name.clone();
            let service_count = config.services.len();
            Ok(format!(
                "配置加载成功：站点={}, 服务数={}",
                site, service_count
            ))
        }
        Err(e) => Err(format!("配置加载失败: {}", e)),
    }
}

/// 解析单个 Request URL
#[tauri::command]
pub fn resolve_request_url(
    url: String,
    gateway_prefix: String,
) -> Result<ResolvedUrl, String> {
    url_resolver::resolve_url(&url, &gateway_prefix).map_err(|e| e.to_string())
}

/// 启动诊断流程（接收前端 WebView 捕获的请求数据）
#[tauri::command]
pub async fn start_diagnosis(
    captured_json: String,
    config_path: String,
) -> Result<String, String> {
    // 解析前端传来的捕获数据
    let captured: CapturedPage =
        serde_json::from_str(&captured_json).map_err(|e| format!("解析捕获数据失败: {}", e))?;

    // 加载配置
    let config =
        CollectorConfig::load(&config_path).map_err(|e| format!("加载配置失败: {}", e))?;

    // 运行诊断流程
    let runner = DiagnosisRunner::new(config, captured);
    match runner.run().await {
        Ok(output_path) => Ok(output_path),
        Err(e) => Err(format!("诊断执行失败: {}", e)),
    }
}

/// 获取诊断状态
#[tauri::command]
pub fn get_diagnosis_status() -> DiagnosisStatus {
    DiagnosisStatus::default()
}
