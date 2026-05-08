use diag_core::config::CollectorConfig;
use diag_core::models::{CapturedPage, ResolvedUrl};
use diag_core::url_resolver;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::State;

use crate::db_collector::DbCollector;
use crate::deployment::{self, DatabaseDeployment, DeploymentManifest, ServiceDeployment, ValidationResult};
use crate::diagnosis::DiagnosisRunner;
use crate::ssh_collector;
use crate::validator;

/// 全局应用状态
pub struct AppState {
    pub manifest: Mutex<Option<DeploymentManifest>>,
    pub config: Mutex<Option<CollectorConfig>>,
    pub validated: Mutex<bool>,
}

// ═══════════════════════════════════════
// 第一步：部署文档导入
// ═══════════════════════════════════════

/// 生成服务部署模板 CSV
#[tauri::command]
pub fn generate_service_template() -> String {
    deployment::generate_service_template()
}

/// 生成数据库部署模板 CSV
#[tauri::command]
pub fn generate_db_template() -> String {
    deployment::generate_db_template()
}

/// 导出模板到文件
#[tauri::command]
pub fn export_template(output_dir: String) -> Result<serde_json::Value, String> {
    let svc_path = std::path::Path::new(&output_dir).join("服务部署模板.csv");
    let db_path = std::path::Path::new(&output_dir).join("数据库部署模板.csv");

    // 确保目录存在
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("创建目录失败: {}", e))?;

    // 写 BOM + CSV（兼容 Excel 中文）
    let bom = "\u{FEFF}";
    std::fs::write(&svc_path, format!("{}{}", bom, deployment::generate_service_template()))
        .map_err(|e| format!("写入服务模板失败: {}", e))?;
    std::fs::write(&db_path, format!("{}{}", bom, deployment::generate_db_template()))
        .map_err(|e| format!("写入数据库模板失败: {}", e))?;

    Ok(serde_json::json!({
        "serviceTemplate": svc_path.to_string_lossy(),
        "dbTemplate": db_path.to_string_lossy(),
    }))
}

/// 导入服务部署 CSV
#[tauri::command]
pub fn import_service_csv(
    csv_content: String,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    // 去掉 BOM
    let content = csv_content.trim_start_matches('\u{FEFF}');

    let services =
        deployment::parse_service_csv(content).map_err(|e| format!("解析服务部署文件失败: {}", e))?;

    let summary: Vec<serde_json::Value> = services
        .iter()
        .map(|s| {
            serde_json::json!({
                "projectName": s.project_name,
                "serverIp": s.server_ip,
                "sshUser": s.ssh_username,
                "sshPort": s.ssh_port,
                "logPath": s.log_path,
                "logPattern": s.log_pattern,
            })
        })
        .collect();

    // 更新 manifest
    let mut manifest = state.manifest.lock().unwrap();
    if manifest.is_none() {
        *manifest = Some(DeploymentManifest {
            site_name: String::new(),
            system: "pcm".to_string(),
            gateway_prefix: "/gateway".to_string(),
            services: Vec::new(),
            databases: Vec::new(),
        });
    }
    if let Some(ref mut m) = *manifest {
        m.services = services;
    }

    Ok(serde_json::json!({
        "serviceCount": summary.len(),
        "services": summary,
    }))
}

/// 导入数据库部署 CSV
#[tauri::command]
pub fn import_db_csv(
    csv_content: String,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let content = csv_content.trim_start_matches('\u{FEFF}');

    let databases =
        deployment::parse_db_csv(content).map_err(|e| format!("解析数据库部署文件失败: {}", e))?;

    let summary: Vec<serde_json::Value> = databases
        .iter()
        .map(|d| {
            serde_json::json!({
                "dbType": d.db_type,
                "host": d.host,
                "port": d.port,
                "username": d.username,
                "database": d.database,
            })
        })
        .collect();

    let mut manifest = state.manifest.lock().unwrap();
    if manifest.is_none() {
        *manifest = Some(DeploymentManifest {
            site_name: String::new(),
            system: "pcm".to_string(),
            gateway_prefix: "/gateway".to_string(),
            services: Vec::new(),
            databases: Vec::new(),
        });
    }
    if let Some(ref mut m) = *manifest {
        m.databases = databases;
    }

    Ok(serde_json::json!({
        "databaseCount": summary.len(),
        "databases": summary,
    }))
}

/// 设置站点名称和网关前缀
#[tauri::command]
pub fn set_site_info(
    site_name: String,
    gateway_prefix: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let mut manifest = state.manifest.lock().unwrap();
    if let Some(ref mut m) = *manifest {
        m.site_name = site_name.clone();
        m.gateway_prefix = gateway_prefix;
        Ok(format!("站点信息已设置: {}", site_name))
    } else {
        Err("请先导入部署文档".to_string())
    }
}

// ═══════════════════════════════════════
// 第二步：连通性校验
// ═══════════════════════════════════════

/// 校验所有服务的 SSH 连通性 + 日志路径
#[tauri::command]
pub async fn validate_services(
    state: State<'_, AppState>,
) -> Result<Vec<ValidationResult>, String> {
    let services = {
        let manifest = state.manifest.lock().unwrap();
        let manifest = manifest.as_ref().ok_or("请先导入部署文档".to_string())?;
        if manifest.services.is_empty() {
            return Err("没有服务配置可校验".to_string());
        }
        manifest.services.clone()
    };

    let results = validator::validate_all_services(&services).await;
    Ok(results)
}

/// 校验单个服务
#[tauri::command]
pub async fn validate_single_service(
    service_index: usize,
    state: State<'_, AppState>,
) -> Result<ValidationResult, String> {
    let svc = {
        let manifest = state.manifest.lock().unwrap();
        let manifest = manifest.as_ref().ok_or("请先导入部署文档".to_string())?;
        manifest.services.get(service_index)
            .ok_or(format!("服务索引 {} 越界", service_index))?
            .clone()
    };

    Ok(validator::validate_service(&svc).await)
}

/// 校验所有数据库连通性
#[tauri::command]
pub async fn validate_databases(
    state: State<'_, AppState>,
) -> Result<Vec<ValidationResult>, String> {
    let databases = {
        let manifest = state.manifest.lock().unwrap();
        let manifest = manifest.as_ref().ok_or("请先导入部署文档".to_string())?;
        if manifest.databases.is_empty() {
            return Err("没有数据库配置可校验".to_string());
        }
        manifest.databases.clone()
    };

    let results = validator::validate_all_databases(&databases).await;
    Ok(results)
}

/// 确认校验完成，生成 CollectorConfig
#[tauri::command]
pub fn confirm_validation(
    state: State<'_, AppState>,
) -> Result<String, String> {
    let manifest = state.manifest.lock().unwrap();
    let manifest = manifest.as_ref().ok_or("请先导入部署文档")?;

    let config = deployment::manifest_to_collector_config(manifest);
    let service_count = config.services.len();

    *state.config.lock().unwrap() = Some(config);
    *state.validated.lock().unwrap() = true;

    Ok(format!(
        "配置已确认：{} 个服务，数据库 {}:{}",
        service_count,
        manifest.databases.first().map(|d| d.host.as_str()).unwrap_or("未配置"),
        manifest.databases.first().map(|d| d.port).unwrap_or(0),
    ))
}

// ═══════════════════════════════════════
// 第三步：URL 采集 + 诊断
// ═══════════════════════════════════════

/// 解析单个 Request URL
#[tauri::command]
pub fn resolve_request_url(url: String, gateway_prefix: String) -> Result<ResolvedUrl, String> {
    url_resolver::resolve_url(&url, &gateway_prefix).map_err(|e| e.to_string())
}

/// 启动完整诊断流程
#[tauri::command]
pub async fn start_diagnosis(
    captured_json: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let config = {
        let validated = *state.validated.lock().unwrap();
        if !validated {
            return Err("请先完成部署校验".to_string());
        }
        let config_guard = state.config.lock().unwrap();
        config_guard.as_ref()
            .ok_or("配置未生成，请先导入并校验部署文档".to_string())?
            .clone()
    };

    let captured: CapturedPage =
        serde_json::from_str(&captured_json).map_err(|e| format!("解析捕获数据失败: {}", e))?;

    tracing::info!(
        "启动诊断: 页面={}, 请求数={}, 站点={}",
        captured.page_url,
        captured.requests.len(),
        config.site.name
    );

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

/// 获取当前配置摘要
#[tauri::command]
pub fn get_config_summary(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let manifest = state.manifest.lock().unwrap();
    let validated = *state.validated.lock().unwrap();

    match manifest.as_ref() {
        Some(m) => Ok(serde_json::json!({
            "siteName": m.site_name,
            "system": m.system,
            "gatewayPrefix": m.gateway_prefix,
            "serviceCount": m.services.len(),
            "databaseCount": m.databases.len(),
            "validated": validated,
            "services": m.services.iter().map(|s| s.project_name.clone()).collect::<Vec<_>>(),
        })),
        None => Err("尚未导入部署文档".to_string()),
    }
}
