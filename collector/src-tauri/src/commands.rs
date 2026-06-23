use chrono::{DateTime, Duration, FixedOffset, Utc};
use diag_core::collector_trait::LogCollector;
use diag_core::config::CollectorConfig;
use diag_core::models::{CapturedPage, ResolvedUrl};
use diag_core::url_resolver;
use std::sync::{Arc, Mutex};
use tauri::State;

use crate::deployment::{self, DeploymentManifest, ValidationResult};
use crate::diagnosis::DiagnosisRunner;
use crate::ssh_log_collector::SshLogCollector;
use crate::validator;

/// 全局应用状态
pub struct AppState {
    pub manifest: Mutex<Option<DeploymentManifest>>,
    pub config: Mutex<Option<CollectorConfig>>,
    pub validated: Mutex<bool>,
    pub scheduler_status: Arc<std::sync::Mutex<crate::scheduler::SchedulerStatus>>,
    pub scheduler_handle: Mutex<Option<crate::scheduler::SchedulerHandle>>,
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

/// 导出模板到用户选择的目录
#[tauri::command]
pub async fn export_template(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    use tauri_plugin_dialog::DialogExt;

    // 弹出目录选择对话框
    let folder = app
        .dialog()
        .file()
        .set_title("选择模板保存位置")
        .blocking_pick_folder();

    let output_dir = match folder {
        Some(path) => path.to_string(),
        None => return Err("用户取消了选择".to_string()),
    };

    let svc_path = std::path::Path::new(&output_dir).join("服务部署模板.csv");
    let db_path = std::path::Path::new(&output_dir).join("数据库部署模板.csv");

    // 写 BOM + CSV（兼容 Excel 中文）
    let bom = "\u{FEFF}";
    std::fs::write(
        &svc_path,
        format!("{}{}", bom, deployment::generate_service_template()),
    )
    .map_err(|e| format!("写入服务模板失败: {}", e))?;
    std::fs::write(
        &db_path,
        format!("{}{}", bom, deployment::generate_db_template()),
    )
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

    let services = deployment::parse_service_csv(content)
        .map_err(|e| format!("解析服务部署文件失败: {}", e))?;

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
            elk: None,
            schedule: None,
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
            elk: None,
            schedule: None,
        });
    }
    if let Some(ref mut m) = *manifest {
        // 保留旧的 schemas：如果新旧数据库连接信息相同（host/port/database），保持已选 schema 不丢失
        // 否则被反复 import_db_csv 会清空用户在 set_selected_schemas 中保存的 PG schema
        let mut new_databases = databases;
        for new_db in new_databases.iter_mut() {
            if let Some(old_db) = m.databases.iter().find(|d| {
                d.host == new_db.host && d.port == new_db.port && d.database == new_db.database
            }) {
                if new_db.schemas.is_empty() && !old_db.schemas.is_empty() {
                    new_db.schemas = old_db.schemas.clone();
                }
            }
        }
        m.databases = new_databases;
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
    if manifest.is_none() {
        // manifest 尚未创建时（未上传 CSV 且纯 ELK 模式），自动初始化
        *manifest = Some(deployment::DeploymentManifest {
            site_name: site_name.clone(),
            system: "pcm".to_string(),
            gateway_prefix: gateway_prefix.clone(),
            services: Vec::new(),
            databases: Vec::new(),
            elk: None,
            schedule: None,
        });
        return Ok(format!("站点信息已初始化: {}", site_name));
    }
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
        manifest
            .services
            .get(service_index)
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

/// 列出数据库服务器上可访问的数据库名列表（在校验通过后调用）
#[tauri::command]
pub async fn list_available_databases(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let db = {
        let manifest = state.manifest.lock().unwrap();
        let manifest = manifest.as_ref().ok_or("请先导入部署文档".to_string())?;
        manifest
            .databases
            .first()
            .ok_or("没有数据库配置".to_string())?
            .clone()
    };

    validator::list_databases(&db).await
}

/// 列出 PostgreSQL 数据库中的 schema 列表（在选定数据库后调用）
#[tauri::command]
pub async fn list_available_schemas(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let db = {
        let manifest = state.manifest.lock().unwrap();
        let manifest = manifest.as_ref().ok_or("请先导入部署文档".to_string())?;
        manifest
            .databases
            .first()
            .ok_or("没有数据库配置".to_string())?
            .clone()
    };

    validator::list_schemas_in_database(&db).await
}

/// 列出数据库服务器上选定数据库/模式下的表列表
#[tauri::command]
pub async fn list_available_tables(
    schemas: Vec<String>,
    state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    let db = {
        let manifest = state.manifest.lock().unwrap();
        let manifest = manifest.as_ref().ok_or("请先导入部署文档".to_string())?;
        manifest
            .databases
            .first()
            .ok_or("没有数据库配置".to_string())?
            .clone()
    };

    validator::list_tables(&db, schemas).await
}

/// 用户在前端选定数据库后，回写到 manifest 的第一个数据库配置中
#[tauri::command]
pub fn set_selected_database(database: String, state: State<'_, AppState>) -> Result<(), String> {
    tracing::info!("[set_selected_database] 接收到: {}", database);
    let mut db_actually_changed = false;
    // 同步更新 manifest
    {
        let mut manifest_guard = state.manifest.lock().unwrap();
        let manifest = manifest_guard.as_mut().ok_or("请先导入部署文档")?;
        if let Some(db) = manifest.databases.first_mut() {
            if db.database != database {
                db_actually_changed = true;
                db.database = database.clone();
                // 仅在数据库真的变化时清空 schemas
                db.schemas = Vec::new();
                tracing::info!("[set_selected_database] 数据库变化，清空 schemas");
            } else {
                tracing::info!(
                    "[set_selected_database] 数据库未变化，保留 schemas: {:?}",
                    db.schemas
                );
            }
        } else {
            return Err("没有数据库配置".to_string());
        }
    }
    // 同步更新 config（仅在数据库真的变化时）
    if db_actually_changed {
        let mut config_guard = state.config.lock().unwrap();
        if let Some(ref mut config) = *config_guard {
            config.database.database = database;
            config.database.schemas = Vec::new();
        }
    }
    Ok(())
}

/// 用户选定多个 schema 后，回写到 manifest 和 config
#[tauri::command]
pub fn set_selected_schemas(
    schemas: Vec<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    tracing::info!(
        "[set_selected_schemas] 接收到 {} 个 schema: {:?}",
        schemas.len(),
        schemas
    );
    // 同步更新 manifest
    {
        let mut manifest_guard = state.manifest.lock().unwrap();
        let manifest = manifest_guard.as_mut().ok_or("请先导入部署文档")?;
        if let Some(db) = manifest.databases.first_mut() {
            db.schemas = schemas.clone();
        } else {
            return Err("没有数据库配置".to_string());
        }
    }
    // 同步更新 config（避免 confirm_validation 时机晚于勾选 schema 时）
    {
        let mut config_guard = state.config.lock().unwrap();
        if let Some(ref mut config) = *config_guard {
            config.database.schemas = schemas.clone();
            tracing::info!(
                "[set_selected_schemas] 已同步到 config.database.schemas: {:?}",
                config.database.schemas
            );
        } else {
            tracing::warn!("[set_selected_schemas] state.config 为空，无法同步");
        }
    }
    Ok(())
}

/// 确认校验完成，生成 CollectorConfig
#[tauri::command]
pub fn confirm_validation(
    output_dir: Option<String>,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let manifest = state.manifest.lock().unwrap();
    let manifest = manifest.as_ref().ok_or("请先导入部署文档")?;

    let mut config = deployment::manifest_to_collector_config(manifest);
    let service_count = config.services.len();

    // 用户在前端配置的输出目录
    if let Some(dir) = output_dir {
        if !dir.is_empty() {
            config.collector.output_dir = dir;
        }
    }

    *state.config.lock().unwrap() = Some(config);
    *state.validated.lock().unwrap() = true;

    Ok(format!(
        "配置已确认：{} 个服务，数据库 {}:{}",
        service_count,
        manifest
            .databases
            .first()
            .map(|d| d.host.as_str())
            .unwrap_or("未配置"),
        manifest.databases.first().map(|d| d.port).unwrap_or(0),
    ))
}

// ═══════════════════════════════════════
// 第三步：WebView 浏览器采集
// ═══════════════════════════════════════

/// 打开诊断浏览器窗口（加载目标 URL，注入 fetch/XHR 拦截 JS）
#[tauri::command]
pub fn open_diag_browser(
    url: String,
    app: tauri::AppHandle,
    captured_store: State<'_, std::sync::Arc<crate::webview_capture::CapturedDataStore>>,
) -> Result<String, String> {
    // 清除上一次的捕获数据
    *captured_store.data.lock().unwrap() = None;

    crate::webview_capture::open_diagnostic_window(&app, &url)?;
    Ok(format!("诊断浏览器已打开: {}", url))
}

/// 从诊断浏览器收集捕获的请求数据
/// 多重备选机制确保数据回传：XHR → fetch → iframe navigation
#[tauri::command]
pub async fn collect_diag_data(
    app: tauri::AppHandle,
    captured_store: State<'_, std::sync::Arc<crate::webview_capture::CapturedDataStore>>,
) -> Result<String, String> {
    use tauri::Manager;

    // 清除旧数据
    *captured_store.data.lock().unwrap() = None;

    // 触发诊断窗口发送数据（通过 custom protocol）
    crate::webview_capture::trigger_data_collection(&app)?;

    // 等待数据到达（最多等 3 秒）
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let data = captured_store.data.lock().unwrap();
        if data.is_some() {
            let json = data.clone().unwrap();
            tracing::info!("已收集诊断数据: {} bytes", json.len());
            return Ok(json);
        }
    }

    // Fallback 1: 再次触发
    tracing::warn!("首次 custom protocol 超时，重试...");
    let window = app
        .get_webview_window("diagnostic")
        .ok_or("诊断窗口未打开")?;

    let _ = window.eval(
        "try { window.__sendDiagData(); } catch(e) { console.error('sendDiagData failed:', e); }",
    );

    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let data = captured_store.data.lock().unwrap();
        if data.is_some() {
            let json = data.clone().unwrap();
            tracing::info!("已收集诊断数据(重试): {} bytes", json.len());
            return Ok(json);
        }
    }

    // Fallback 2: 通过 iframe src 导航触发 custom protocol（某些 WKWebView 版本只允许导航式请求）
    tracing::warn!("XHR/fetch 均超时，尝试 iframe 导航方式...");
    let _ = window.eval(r#"
        (function() {
            try {
                var data = window.__getDiagData();
                var form = document.createElement('form');
                form.method = 'POST';
                form.action = 'diag://collect';
                form.target = '_diag_frame';
                form.style.display = 'none';
                var input = document.createElement('input');
                input.type = 'hidden';
                input.name = 'data';
                input.value = data;
                form.appendChild(input);
                var iframe = document.getElementById('_diag_frame') || document.createElement('iframe');
                iframe.id = '_diag_frame';
                iframe.name = '_diag_frame';
                iframe.style.display = 'none';
                document.body.appendChild(iframe);
                document.body.appendChild(form);
                form.submit();
                setTimeout(function(){ form.remove(); }, 500);
            } catch(e) { console.error('[Smart-Diag] iframe fallback failed:', e); }
        })();
    "#);

    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let data = captured_store.data.lock().unwrap();
        if data.is_some() {
            let json = data.clone().unwrap();
            tracing::info!("已收集诊断数据(iframe): {} bytes", json.len());
            return Ok(json);
        }
    }

    // Fallback 3: 如果所有 protocol 方式都失败，直接通过 eval 获取数据存入标题
    // 这是最后手段——数据量大时标题会被截断，但至少能获得部分数据
    tracing::warn!("所有 protocol 方式均失败，尝试 title 中转...");
    let _ = window.eval(
        r#"
        (function() {
            try {
                var data = window.__getDiagData();
                document.title = '__DIAG_DATA_START__' + data + '__DIAG_DATA_END__';
            } catch(e) {}
        })();
    "#,
    );

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    if let Ok(title) = window.title() {
        if let Some(start) = title.find("__DIAG_DATA_START__") {
            let data_start = start + "__DIAG_DATA_START__".len();
            if let Some(end) = title.find("__DIAG_DATA_END__") {
                let json = title[data_start..end].to_string();
                if !json.is_empty() {
                    tracing::info!("已收集诊断数据(title中转): {} bytes", json.len());
                    return Ok(json);
                }
            }
        }
    }

    Err("采集数据超时。diag:// 协议在当前环境不可用，请确认诊断浏览器窗口仍在运行。".to_string())
}

/// 获取当前捕获的请求计数（实时轮询用）
/// 优先读 Rust 侧计数器（由 diag://count 更新），若为 0 则从诊断窗口标题解析计数
#[tauri::command]
pub async fn get_capture_count(
    count: State<'_, std::sync::Arc<std::sync::Mutex<usize>>>,
    app: tauri::AppHandle,
) -> Result<usize, String> {
    use tauri::Manager;
    let rust_count = *count.lock().unwrap();
    if rust_count > 0 {
        return Ok(rust_count);
    }
    // Fallback: 从诊断窗口标题读取计数（JS 在标题中嵌入 [DIAG:N]）
    if let Some(window) = app.get_webview_window("diagnostic") {
        if let Ok(title) = window.title() {
            if let Some(start) = title.find("[DIAG:") {
                let rest = &title[start + 6..];
                if let Some(end) = rest.find(']') {
                    if let Ok(n) = rest[..end].parse::<usize>() {
                        if n > 0 {
                            *count.lock().unwrap() = n;
                            return Ok(n);
                        }
                    }
                }
            }
        }
    }
    Ok(0)
}

/// 重置采集数据（登录完成后调用，清除登录期间的请求，从头开始捕获业务请求）
#[tauri::command]
pub fn reset_capture_data(
    target_url: Option<String>,
    captured_store: State<'_, std::sync::Arc<crate::webview_capture::CapturedDataStore>>,
    count: State<'_, std::sync::Arc<std::sync::Mutex<usize>>>,
    app: tauri::AppHandle,
) -> String {
    use tauri::Manager;
    // 清除已缓存的捕获数据
    *captured_store.data.lock().unwrap() = None;
    // 重置计数
    *count.lock().unwrap() = 0;
    // 在诊断 WebView 中清空并（可选）重新导航到目标页面
    if let Some(window) = app.get_webview_window("diagnostic") {
        let _ = window.eval("window.__diag_requests = []; window.__diag_page_url = location.href;");
        // 如果提供了目标 URL，重新导航以捕获页面初始加载的 API 请求
        if let Some(ref url) = target_url {
            if !url.is_empty() {
                let nav_js = format!("window.location.href = '{}';", url.replace('\'', "\\'"));
                let _ = window.eval(&nav_js);
                tracing::info!("重置采集并重新导航到: {}", url);
                return "已重置并重新加载目标页面，请等待页面加载完成后操作".to_string();
            }
        }
    }
    tracing::info!("采集数据已重置，重新开始捕获");
    "已重置采集数据，请操作目标页面复现问题".to_string()
}

/// 关闭诊断浏览器窗口
#[tauri::command]
pub fn close_diag_browser(app: tauri::AppHandle) -> String {
    crate::webview_capture::close_diagnostic_window(&app);
    "诊断浏览器已关闭".to_string()
}

/// 解析单个 Request URL
#[tauri::command]
pub fn resolve_request_url(url: String, gateway_prefix: String) -> Result<ResolvedUrl, String> {
    url_resolver::resolve_url(&url, &gateway_prefix).map_err(|e| e.to_string())
}

/// 实时模式：从 manifest 最新 ELK 配置构建 ElkCollector，保证字段映射与快速诊断一致
fn build_elk_config_from_manifest(
    manifest: &crate::deployment::DeploymentManifest,
) -> Option<diag_core::config::ElkConfig> {
    use diag_core::config::{ElkConfig, FieldMapping};
    manifest.elk.as_ref().map(|e| ElkConfig {
        address: e.address.clone(),
        index_pattern: e.index_pattern.clone(),
        username: e.username.clone(),
        password: e.password.clone(),
        timeout_secs: e.timeout_secs.unwrap_or(30),
        max_hits_per_trace: e.max_hits_per_trace.unwrap_or(2000),
        field_mapping: FieldMapping {
            timestamp: e
                .field_timestamp
                .clone()
                .unwrap_or_else(|| "@timestamp".into()),
            level: e.field_level.clone().unwrap_or_else(|| "level".into()),
            service: e
                .field_service
                .clone()
                .unwrap_or_else(|| "serviceName".into()),
            trace_id: e.field_trace_id.clone().unwrap_or_else(|| "x0".into()),
            message: e.field_message.clone().unwrap_or_else(|| "msg".into()),
            exception: "exception".into(),
            stack_trace: "stackTrace".into(),
            thread: "thread".into(),
        },
    })
}

/// 启动完整诊断流程（实时模式）
#[tauri::command]
pub async fn start_diagnosis(
    captured_json: String,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    let mut config = {
        let validated = *state.validated.lock().unwrap();
        if !validated {
            return Err("请先完成部署校验".to_string());
        }
        let config_guard = state.config.lock().unwrap();
        config_guard
            .as_ref()
            .ok_or("配置未生成，请先导入并校验部署文档".to_string())?
            .clone()
    };

    // 从 manifest 读取最新 ELK 配置，覆盖 config.elk（避免旧 config 中 index_pattern 缺通配符等问题）
    {
        let manifest = state.manifest.lock().unwrap();
        if let Some(ref m) = *manifest {
            if let Some(elk_cfg) = build_elk_config_from_manifest(m) {
                tracing::info!(
                    "[实时诊断] 从 manifest 刷新 ELK 配置: index={}, trace_id_field={}",
                    elk_cfg.index_pattern,
                    elk_cfg.field_mapping.trace_id
                );
                config.elk = Some(elk_cfg);
            }
        }
    }

    resolve_output_dir(&app, &mut config);

    let captured: CapturedPage =
        serde_json::from_str(&captured_json).map_err(|e| format!("解析捕获数据失败: {}", e))?;

    let services: Vec<String> = captured
        .requests
        .iter()
        .filter_map(|r| url_resolver::resolve_url(&r.url, &config.gateway.prefix).ok())
        .map(|r| r.service)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    tracing::info!(
        "启动诊断: 页面={}, 请求数={}, 站点={}",
        captured.page_url,
        captured.requests.len(),
        config.site.name
    );

    // 优先使用 ELK，不可用时降级到 SSH
    let log_collector: Box<dyn diag_core::collector_trait::LogCollector> =
        if let Some(elk_cfg) = &config.elk {
            match crate::elk_collector::ElkCollector::new(elk_cfg.clone()).await {
                Ok(elk) => {
                    tracing::info!("使用 ELK 采集日志");
                    Box::new(elk)
                }
                Err(e) => {
                    tracing::warn!("ELK 不可用 ({}), 降级 SSH", e);
                    Box::new(SshLogCollector::new(
                        config.ssh.clone(),
                        config.services.clone(),
                        config.collector.max_log_lines,
                    ))
                }
            }
        } else {
            Box::new(SshLogCollector::new(
                config.ssh.clone(),
                config.services.clone(),
                config.collector.max_log_lines,
            ))
        };

    let runner = DiagnosisRunner::new(config, captured, log_collector);
    match runner.run().await {
        Ok(output_path) => {
            tracing::info!("诊断完成: {}", output_path);
            Ok(serde_json::json!({
                "outputPath": output_path,
                "services": services,
            }))
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

/// 获取桌面路径
#[tauri::command]
pub fn get_desktop_path() -> Result<String, String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "无法获取用户主目录".to_string())?;

    Ok(format!("{}/Desktop", home))
}

/// 弹出原生文件夹选择器，返回用户选中的路径
#[tauri::command]
pub async fn pick_output_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;

    let folder = app
        .dialog()
        .file()
        .set_title("选择诊断包保存位置")
        .blocking_pick_folder();

    Ok(folder.map(|p| p.to_string()))
}

// ═══════════════════════════════════════
// ELK 相关命令
// ═══════════════════════════════════════

/// 设置 ELK 配置
#[tauri::command]
pub fn set_elk_config(
    elk: crate::deployment::ElkDeployment,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let mut manifest = state.manifest.lock().unwrap();
    if let Some(ref mut m) = *manifest {
        m.elk = Some(elk);
        Ok("ELK 配置已设置".to_string())
    } else {
        // 允许在没有服务配置时也能设置 ELK
        *manifest = Some(crate::deployment::DeploymentManifest {
            site_name: String::new(),
            system: "pcm".to_string(),
            gateway_prefix: "/gateway".to_string(),
            services: Vec::new(),
            databases: Vec::new(),
            elk: Some(elk),
            schedule: None,
        });
        Ok("ELK 配置已设置（新建配置）".to_string())
    }
}

/// 测试 ELK 连接（版本检测 + 样本查询）
/// 注意：依赖 elk_collector::ElkCollector::es_major_version() 方法（由 Agent B 实现）
#[tauri::command]
pub async fn test_elk_connection(
    address: String,
    index_pattern: String,
    username: Option<String>,
    password: Option<String>,
) -> Result<serde_json::Value, String> {
    use diag_core::config::{ElkConfig, FieldMapping};

    let config = ElkConfig {
        address,
        index_pattern,
        username,
        password,
        timeout_secs: 10,
        max_hits_per_trace: 1,
        field_mapping: FieldMapping::default(),
    };

    match crate::elk_collector::ElkCollector::new(config).await {
        Ok(collector) => Ok(serde_json::json!({
            "success": true,
            "message": "ELK 连接成功",
            "esVersion": collector.es_major_version(),
        })),
        Err(e) => Err(format!("ELK 连接失败: {}", e)),
    }
}

/// 设置定时任务配置
#[tauri::command]
pub fn set_schedule_config(
    schedule: crate::deployment::ScheduleDeployment,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let mut manifest = state.manifest.lock().unwrap();
    if let Some(ref mut m) = *manifest {
        m.schedule = Some(schedule);
        Ok("定时任务配置已设置".to_string())
    } else {
        Err("请先完成基础配置".to_string())
    }
}

// ═══════════════════════════════════════
// 定时巡检调度器
// ═══════════════════════════════════════

/// 启动定时巡检调度器
#[tauri::command]
pub async fn start_scheduler(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<String, String> {
    // 检查是否已在运行
    {
        let status = state.scheduler_status.lock().unwrap();
        if status.running {
            return Err("调度器已在运行中".to_string());
        }
    }

    let mut config = {
        let cfg = state.config.lock().unwrap();
        cfg.as_ref().ok_or("请先完成配置")?.clone()
    };

    // 从 manifest 读取最新 ELK 配置，与其他模式保持一致
    {
        let manifest = state.manifest.lock().unwrap();
        if let Some(ref m) = *manifest {
            if let Some(elk_cfg) = build_elk_config_from_manifest(m) {
                tracing::info!(
                    "[调度器] 从 manifest 刷新 ELK 配置: index={}, trace_id_field={}",
                    elk_cfg.index_pattern,
                    elk_cfg.field_mapping.trace_id
                );
                config.elk = Some(elk_cfg);
            }
        }
    }

    resolve_output_dir(&app, &mut config);

    let handle = crate::scheduler::start(app, config, state.scheduler_status.clone())
        .map_err(|e| e.to_string())?;

    *state.scheduler_handle.lock().unwrap() = Some(handle);

    let interval = {
        let status = state.scheduler_status.lock().unwrap();
        status.next_run_at.clone().unwrap_or_default()
    };

    Ok(format!("定时巡检已启动，下次运行: {}", interval))
}

/// 停止定时巡检调度器
#[tauri::command]
pub fn stop_scheduler(state: State<'_, AppState>) -> String {
    if let Some(ref mut handle) = *state.scheduler_handle.lock().unwrap() {
        handle.stop();
        tracing::info!("调度器已发送停止信号");
        "调度器正在停止...".to_string()
    } else {
        "调度器未运行".to_string()
    }
}

/// 获取调度器当前状态
#[tauri::command]
pub fn get_scheduler_status(state: State<'_, AppState>) -> crate::scheduler::SchedulerStatus {
    state.scheduler_status.lock().unwrap().clone()
}

/// 启动历史模式诊断（按关键词 / traceId 直查 + 时间窗口）
/// keywords 中若某项看起来像 traceId（含点号的 hex 字符串），自动切换为 term 精确查询
#[tauri::command]
pub async fn start_historical_diagnosis(
    keywords: Vec<String>,
    time_start: String,
    time_end: String,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    /// 向前端推送诊断步骤进度事件
    fn emit_step(app: &tauri::AppHandle, step: &str, detail: &str, status: &str) {
        use tauri::Emitter;
        let _ = app.emit(
            "history-step",
            serde_json::json!({
                "step":   step,
                "detail": detail,
                "status": status,   // "running" | "done" | "error" | "skip"
            }),
        );
    }

    let mut config = {
        let cfg = state.config.lock().unwrap();
        cfg.as_ref().ok_or("请先完成配置")?.clone()
    };

    // 从 manifest 读取最新 ELK 配置，与实时/快速模式保持一致
    {
        let manifest = state.manifest.lock().unwrap();
        if let Some(ref m) = *manifest {
            if let Some(elk_cfg) = build_elk_config_from_manifest(m) {
                tracing::info!(
                    "[历史诊断] 从 manifest 刷新 ELK 配置: index={}, trace_id_field={}",
                    elk_cfg.index_pattern,
                    elk_cfg.field_mapping.trace_id
                );
                config.elk = Some(elk_cfg);
            }
        }
    }

    resolve_output_dir(&app, &mut config);

    if config.elk.is_none() {
        return Err("历史模式需要 ELK 配置，请在 Configure 页面填写 ELK 地址".to_string());
    }

    let elk_config = config.elk.as_ref().unwrap().clone();
    let window = diag_core::models::TimeWindow {
        start: time_start.clone(),
        end: time_end.clone(),
    };

    // ── Step 1：连接 ELK ──
    emit_step(
        &app,
        "elk-connect",
        &format!("正在连接 ELK：{}", elk_config.address),
        "running",
    );

    let elk_collector = crate::elk_collector::ElkCollector::new(elk_config.clone())
        .await
        .map_err(|e| {
            emit_step(
                &app,
                "elk-connect",
                &format!("ELK 连接失败：{}", e),
                "error",
            );
            format!("ELK 连接失败: {}", e)
        })?;
    emit_step(
        &app,
        "elk-connect",
        &format!(
            "ELK 连接成功（ES 版本 {}）",
            elk_collector.es_major_version()
        ),
        "done",
    );

    // ── Step 2：查询关键词日志 ──
    let kw_display = if keywords.is_empty() {
        "（无关键词）".to_string()
    } else {
        keywords.join(" · ")
    };
    emit_step(
        &app,
        "elk-query",
        &format!(
            "查询关键词 [{}]，时间范围 {} ~ {}",
            kw_display, &time_start, &time_end
        ),
        "running",
    );

    // 自动识别 traceId 关键词（含点号的 16 进制字符串）→ 精确 term 查询
    // 普通文本关键词 → query_string 全文搜索
    let looks_like_trace_id = |s: &str| -> bool {
        s.contains('.') && s.chars().all(|c| c.is_ascii_hexdigit() || c == '.')
    };
    let (trace_id_keys, text_keys): (Vec<&str>, Vec<&str>) = keywords
        .iter()
        .map(|k| k.as_str())
        .partition(|k| looks_like_trace_id(k));

    let logs = if !trace_id_keys.is_empty() {
        // traceId 直查：用 terms 精确匹配，不受 Lucene 点号解析影响
        let tids: Vec<String> = trace_id_keys.iter().map(|s| s.to_string()).collect();
        emit_step(
            &app,
            "elk-query",
            &format!("traceId 精确查询：{}", tids.join(", ")),
            "running",
        );
        elk_collector
            .query_by_exact_trace_ids(&tids, &window)
            .await
            .map_err(|e| {
                emit_step(
                    &app,
                    "elk-query",
                    &format!("traceId 查询失败：{}", e),
                    "error",
                );
                format!("ELK 查询失败: {}", e)
            })?
    } else {
        // 普通关键词全文搜索
        let text_keywords: Vec<String> = text_keys.iter().map(|s| s.to_string()).collect();
        elk_collector
            .query_by_keywords(&text_keywords, None, &window)
            .await
            .map_err(|e| {
                emit_step(&app, "elk-query", &format!("ELK 查询失败：{}", e), "error");
                format!("ELK 查询失败: {}", e)
            })?
    };

    if logs.is_empty() {
        emit_step(
            &app,
            "elk-query",
            "未找到日志 — 请检查：关键词是否正确、时间范围是否覆盖问题发生时间、索引名称是否正确",
            "error",
        );
        return Err("未查到相关日志，请调整关键词或时间范围".to_string());
    }

    // 统计日志级别分布
    let error_count = logs.iter().filter(|l| l.level == "ERROR").count();
    let warn_count = logs.iter().filter(|l| l.level == "WARN").count();
    emit_step(
        &app,
        "elk-query",
        &format!(
            "找到 {} 条日志（ERROR: {}，WARN: {}，其他: {}）",
            logs.len(),
            error_count,
            warn_count,
            logs.len().saturating_sub(error_count + warn_count)
        ),
        "done",
    );

    // ── Step 3：提取 traceId ──
    emit_step(
        &app,
        "extract-trace",
        "正在从日志中提取 traceId...",
        "running",
    );

    let trace_ids: Vec<String> = logs
        .iter()
        .filter_map(|l| l.trace_id.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let no_trace_count = logs.iter().filter(|l| l.trace_id.is_none()).count();
    if trace_ids.is_empty() {
        emit_step(
            &app,
            "extract-trace",
            &format!(
                "全部 {} 条日志均无 traceId — 请检查网关是否透传 x-trace header",
                logs.len()
            ),
            "error",
        );
        return Err("日志中无 traceId，无法关联完整链路".to_string());
    }
    emit_step(
        &app,
        "extract-trace",
        &format!(
            "提取到 {} 个 traceId{}",
            trace_ids.len(),
            if no_trace_count > 0 {
                format!("（{} 条日志无 traceId 已跳过）", no_trace_count)
            } else {
                String::new()
            }
        ),
        "done",
    );

    tracing::info!(
        "历史诊断：关键词查到 {} 条日志，{} 个 traceId",
        logs.len(),
        trace_ids.len()
    );

    // ── Step 4：按 traceId 采集完整链路日志 ──
    emit_step(
        &app,
        "collect-logs",
        &format!(
            "按 {} 个 traceId 从 ELK 采集完整链路日志...",
            trace_ids.len()
        ),
        "running",
    );

    let log_collector: Box<dyn diag_core::collector_trait::LogCollector> = Box::new(
        crate::elk_collector::ElkCollector::new(elk_config)
            .await
            .map_err(|e| format!("ELK 初始化失败: {}", e))?,
    );

    // DiagnosisRunner 内部会采集完整链路 + SQL + 打包
    // 在 run() 前先通知前端"采集中"
    let runner = crate::diagnosis::DiagnosisRunner::new_historical(
        config.clone(),
        log_collector,
        trace_ids.clone(),
    );

    emit_step(
        &app,
        "collect-logs",
        "traceId 关联日志采集中（ELK 并发查询）...",
        "running",
    );

    // ── Step 5：运行诊断流程（日志 + SQL + 打包）──
    // 由 DiagnosisRunner::run() 统一执行，完成后再细化步骤提示
    emit_step(
        &app,
        "query-sql",
        "查询 performance_schema 慢 SQL...",
        "running",
    );
    emit_step(&app, "masking", "隐私脱敏中...", "running");
    emit_step(&app, "package", "打包 diagnosis.zip...", "running");

    let output_path = runner.run().await.map_err(|e| {
        emit_step(&app, "package", &format!("诊断失败：{}", e), "error");
        format!("诊断执行失败: {}", e)
    })?;

    // ── 完成：更新各步骤为 done ──
    emit_step(
        &app,
        "collect-logs",
        &format!("链路日志采集完成，涉及 {} 个 traceId", trace_ids.len()),
        "done",
    );
    emit_step(&app, "query-sql", "慢 SQL 查询完成", "done");
    emit_step(&app, "masking", "隐私脱敏完成", "done");
    emit_step(&app, "package", &format!("已生成：{}", output_path), "done");

    Ok(serde_json::json!({
        "outputPath":  output_path,
        "logCount":    logs.len(),
        "traceCount":  trace_ids.len(),
        "errorCount":  error_count,
        "warnCount":   warn_count,
        "noTraceCount": no_trace_count,
    }))
}

// ═══════════════════════════════════════
// 配置持久化
// ═══════════════════════════════════════

fn app_data_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    use tauri::Manager;
    app.path()
        .app_data_dir()
        .map_err(|e| format!("无法获取应用数据目录: {}", e))
}

fn resolve_output_dir(app: &tauri::AppHandle, config: &mut CollectorConfig) {
    use tauri::Manager;
    // 如果 config 中已有有效的绝对路径（用户在前端配置的），保留它
    let existing = &config.collector.output_dir;
    if !existing.is_empty() && existing != "./diagnosis-output" {
        let p = std::path::Path::new(existing);
        if p.is_absolute() {
            return;
        }
    }
    // 否则使用 app_data_dir 作为默认
    if let Ok(dir) = app.path().app_data_dir() {
        config.collector.output_dir = dir.join("diagnosis-output").to_string_lossy().to_string();
    }
}

/// 保存当前配置到本地（JSON 文件），供下次启动自动加载
#[tauri::command]
pub fn save_config_to_disk(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let manifest = state.manifest.lock().unwrap();
    let manifest = manifest.as_ref().ok_or("尚未配置，无可保存内容")?;
    let dir = app_data_dir(&app)?;
    crate::config_store::save_config(&dir, manifest)
        .map(|p| format!("配置已保存到: {}", p.display()))
        .map_err(|e| format!("保存失败: {}", e))
}

/// 检查并加载本地已保存的配置，返回完整 manifest JSON 供前端回填表单
#[tauri::command]
pub fn load_config_from_disk(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let dir = app_data_dir(&app)?;
    if !crate::config_store::has_saved_config(&dir) {
        return Err("没有已保存的配置".to_string());
    }
    let manifest =
        crate::config_store::load_config(&dir).map_err(|e| format!("加载失败: {}", e))?;

    // 更新 AppState（同时生成 CollectorConfig 供后续诊断使用）
    let config = deployment::manifest_to_collector_config(&manifest);
    *state.manifest.lock().unwrap() = Some(manifest.clone());
    *state.config.lock().unwrap() = Some(config);
    *state.validated.lock().unwrap() = false; // 加载后需重新校验

    tracing::info!("已从磁盘加载配置: 站点={}", manifest.site_name);

    // 返回结构化 JSON，前端用于回填各字段
    Ok(serde_json::to_value(&manifest).unwrap_or_default())
}

/// 清空本地配置文件，同时重置 AppState
#[tauri::command]
pub fn clear_saved_config(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let dir = app_data_dir(&app)?;
    crate::config_store::delete_config(&dir).map_err(|e| format!("清空失败: {}", e))?;

    // 重置运行时状态
    *state.manifest.lock().unwrap() = None;
    *state.config.lock().unwrap() = None;
    *state.validated.lock().unwrap() = false;

    tracing::info!("本地配置已清空");
    Ok("配置已清空".to_string())
}

// ═══════════════════════════════════════
// 快速诊断模式
// ═══════════════════════════════════════

/// 快速诊断：输入单个 traceId，直接从 ELK 查询日志并输出 TXT 格式 ZIP
#[tauri::command]
pub async fn start_quick_diagnosis(
    trace_id: String,
    field_trace_id: Option<String>,
    field_message: Option<String>,
    index_pattern: Option<String>,
    output_dir: Option<String>,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    use diag_core::config::{ElkConfig, FieldMapping};

    // 设置 panic hook 以便在崩溃时记录更多信息
    tracing::info!(
        "快速诊断启动: trace_id={}, field_trace_id={:?}, field_message={:?}",
        trace_id,
        field_trace_id,
        field_message
    );
    tracing::info!("快速诊断 — 各阶段开始前打日志以定位闪退位置");

    fn emit_step(app: &tauri::AppHandle, step: &str, detail: &str, status: &str) {
        use tauri::Emitter;
        let _ = app.emit(
            "quick-diag-step",
            serde_json::json!({
                "step":   step,
                "detail": detail,
                "status": status,
            }),
        );
    }

    if trace_id.trim().is_empty() {
        return Err("请输入 traceId".to_string());
    }

    // 读取 ELK 配置（从 manifest 的 ElkDeployment 转换）
    let elk_deployment = {
        let manifest = state.manifest.lock().unwrap();
        let manifest = manifest.as_ref().ok_or("请先配置 ELK 信息")?;
        manifest
            .elk
            .as_ref()
            .ok_or("快速诊断需要 ELK 配置")?
            .clone()
    };

    // 读取数据库配置（可选，用于 EXPLAIN）
    let db_config: Option<diag_core::config::DatabaseConfig> = {
        let cfg = state.config.lock().unwrap();
        cfg.as_ref().map(|c| c.database.clone())
    }
    .or_else(|| {
        let manifest = state.manifest.lock().unwrap();
        manifest
            .as_ref()
            .and_then(|m| m.databases.first())
            .map(|db| diag_core::config::DatabaseConfig {
                db_type: db.db_type.clone(),
                host: db.host.clone(),
                port: db.port,
                username: db.username.clone(),
                password: db.password.clone(),
                database: db.database.clone(),
                schemas: db.schemas.clone(),
            })
    });

    if let Some(ref c) = db_config {
        tracing::info!(
            "[快速诊断] DB 配置: type={}, db={}, schemas={:?}",
            c.db_type,
            c.database,
            c.schemas
        );
    }

    // 覆盖字段映射
    let field_tid = field_trace_id.unwrap_or_else(|| "x0".to_string());
    let field_msg = field_message.unwrap_or_else(|| "msg".to_string());

    let idx_pattern = index_pattern.unwrap_or_else(|| elk_deployment.index_pattern.clone());

    let elk_config = ElkConfig {
        address: elk_deployment.address.clone(),
        index_pattern: idx_pattern.clone(),
        username: elk_deployment.username.clone(),
        password: elk_deployment.password.clone(),
        timeout_secs: elk_deployment.timeout_secs.unwrap_or(30),
        max_hits_per_trace: elk_deployment.max_hits_per_trace.unwrap_or(2000),
        field_mapping: FieldMapping {
            timestamp: elk_deployment
                .field_timestamp
                .clone()
                .unwrap_or_else(|| "@timestamp".into()),
            level: elk_deployment
                .field_level
                .clone()
                .unwrap_or_else(|| "level".into()),
            service: elk_deployment
                .field_service
                .clone()
                .unwrap_or_else(|| "serviceName".into()),
            trace_id: field_tid.clone(),
            message: field_msg.clone(),
            exception: "exception".into(),
            stack_trace: "stackTrace".into(),
            thread: "thread".into(),
        },
    };

    emit_step(
        &app,
        "elk-connect",
        &format!(
            "连接 ELK：{}，索引={}，traceId 字段={}, msg 字段={}",
            elk_config.address, idx_pattern, field_tid, field_msg
        ),
        "running",
    );

    // 创建 ELK collector
    let elk_collector = crate::elk_collector::ElkCollector::new(elk_config)
        .await
        .map_err(|e| {
            emit_step(
                &app,
                "elk-connect",
                &format!("ELK 连接失败：{}", e),
                "error",
            );
            format!("ELK 连接失败: {}", e)
        })?;

    emit_step(
        &app,
        "elk-connect",
        &format!("ELK 连接成功（ES {}）", elk_collector.es_major_version()),
        "done",
    );

    // 查询日志
    emit_step(
        &app,
        "elk-query",
        &format!("查询 traceId: {}", trace_id),
        "running",
    );

    let empty_window = diag_core::models::TimeWindow {
        start: String::new(),
        end: String::new(),
    };

    let logs = elk_collector
        .query_by_exact_trace_ids(&[trace_id.clone()], &empty_window)
        .await
        .map_err(|e| {
            emit_step(&app, "elk-query", &format!("查询失败：{}", e), "error");
            format!("ELK 查询失败: {}", e)
        })?;

    if logs.is_empty() {
        emit_step(&app, "elk-query", "未找到相关日志", "error");
        return Err("未查到该 traceId 的日志，请确认 traceId 和字段名正确".to_string());
    }

    emit_step(
        &app,
        "elk-query",
        &format!("找到 {} 条日志", logs.len()),
        "done",
    );

    // 提取 SQL traces
    emit_step(&app, "extract-sql", "从日志中提取 SQL...", "running");
    let sql_traces = crate::sql_extractor::extract_sql_traces(&logs);
    emit_step(
        &app,
        "extract-sql",
        &format!("提取到 {} 条 SQL", sql_traces.len()),
        "done",
    );

    // EXPLAIN + 表统计（如果有数据库配置）
    let mut slow_sqls = Vec::new();
    let mut explain_plans = Vec::new();
    let mut table_stats = Vec::new();
    let mut collection_errors = Vec::new();

    if let Some(ref db_cfg) = db_config {
        if !sql_traces.is_empty() {
            emit_step(&app, "explain", "运行 EXPLAIN 分析...", "running");

            // 设置 5 秒超时，避免连接不可达的数据库时长时间阻塞
            let db_cfg_clone = db_cfg.clone();
            let db_result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                crate::db_collector::DbCollector::new(db_cfg_clone).collect(),
            )
            .await;

            match db_result {
                Ok(Ok((collected_slow_sqls, stats))) => {
                    slow_sqls = collected_slow_sqls;
                    table_stats = stats;
                    // EXPLAIN（同样加超时）
                    let explain_collector =
                        crate::explain_collector::ExplainCollector::new(db_cfg.clone(), 500.0);
                    let explain_result = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        explain_collector.collect_explain_plans(&slow_sqls),
                    )
                    .await;
                    match explain_result {
                        Ok(plans) => {
                            explain_plans = plans;
                        }
                        Err(_) => {
                            collection_errors.push("EXPLAIN 计划收集超时（10s）".to_string());
                        }
                    }

                    // 针对日志 SQL 跑 EXPLAIN（参数拼装后再执行）
                    let log_explain_result = tokio::time::timeout(
                        std::time::Duration::from_secs(15),
                        explain_collector.collect_explain_for_sql_traces(&sql_traces),
                    )
                    .await;
                    match log_explain_result {
                        Ok(plans) => {
                            explain_plans.extend(plans);
                        }
                        Err(_) => {
                            collection_errors
                                .push("日志 SQL 的 EXPLAIN 收集超时（15s）".to_string());
                        }
                    }

                    emit_step(
                        &app,
                        "explain",
                        &format!(
                            "EXPLAIN 完成：{} 个执行计划，{} 张表统计",
                            explain_plans.len(),
                            table_stats.len()
                        ),
                        "done",
                    );
                }
                Ok(Err(e)) => {
                    emit_step(&app, "explain", &format!("数据库查询失败：{}", e), "error");
                    tracing::warn!("快速诊断 DB 阶段失败: {}", e);
                    collection_errors.push(format!("数据库查询失败: {}", e));
                }
                Err(_) => {
                    emit_step(
                        &app,
                        "explain",
                        "数据库连接超时（5s），跳过 EXPLAIN",
                        "error",
                    );
                    tracing::warn!("快速诊断 DB 连接超时，跳过");
                    collection_errors.push("数据库连接超时（5s），跳过 EXPLAIN".to_string());
                }
            }
        }
    } else {
        emit_step(&app, "explain", "未配置数据库，跳过 EXPLAIN", "skip");
    }

    // 打包
    emit_step(&app, "package", "生成 TXT 诊断包...", "running");

    let out_dir = output_dir.unwrap_or_else(|| {
        use tauri::Manager;
        let cfg = state.config.lock().unwrap();
        cfg.as_ref()
            .map(|c| c.collector.output_dir.clone())
            .unwrap_or_else(|| {
                app.path()
                    .app_data_dir()
                    .map(|p| p.join("diagnosis-output").to_string_lossy().to_string())
                    .unwrap_or_else(|_| "./diagnosis-output".to_string())
            })
    });

    std::fs::create_dir_all(&out_dir).map_err(|e| format!("创建输出目录失败: {}", e))?;

    let now = Utc::now();
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let short_tid = if trace_id.len() > 8 {
        &trace_id[..8]
    } else {
        &trace_id
    };
    let filename = format!("quick-diag-{}-{}.zip", short_tid, timestamp);
    let output_path = std::path::Path::new(&out_dir).join(&filename);

    let time_window = quick_diagnosis_time_window(&logs);
    let site_name = state
        .config
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.site.name.clone())
        .unwrap_or_else(|| {
            state
                .manifest
                .lock()
                .unwrap()
                .as_ref()
                .map(|m| m.site_name.clone())
                .unwrap_or_else(|| "unknown".to_string())
        });
    let system = state
        .config
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.site.system.clone())
        .unwrap_or_else(|| {
            state
                .manifest
                .lock()
                .unwrap()
                .as_ref()
                .map(|m| m.system.clone())
                .unwrap_or_else(|| "pcm".to_string())
        });
    let gateway_prefix = state
        .config
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.gateway.prefix.clone())
        .unwrap_or_else(|| {
            state
                .manifest
                .lock()
                .unwrap()
                .as_ref()
                .map(|m| m.gateway_prefix.clone())
                .unwrap_or_else(|| "/gateway".to_string())
        });
    let database_type = db_config
        .as_ref()
        .map(|db| db.db_type.as_str())
        .unwrap_or("unknown");
    let manifest = quick_diagnosis_manifest(
        &trace_id,
        &site_name,
        &system,
        database_type,
        &gateway_prefix,
        &logs,
        &sql_traces,
        &time_window,
        &now,
    );
    let report = quick_diagnosis_report(
        &now,
        "elk",
        logs.len(),
        sql_traces.len(),
        explain_plans.len(),
        collection_errors,
    );

    diag_core::package::build_quick_package_with_manifest(
        &logs,
        &sql_traces,
        &slow_sqls,
        &explain_plans,
        &table_stats,
        &manifest,
        Some(&report),
        None,
        &output_path,
    )
    .map_err(|e| {
        emit_step(&app, "package", &format!("打包失败：{}", e), "error");
        format!("打包失败: {}", e)
    })?;

    let path_str = output_path.to_string_lossy().to_string();
    emit_step(&app, "package", &format!("已生成：{}", path_str), "done");

    Ok(serde_json::json!({
        "outputPath": path_str,
        "logCount": logs.len(),
        "sqlCount": sql_traces.len(),
        "explainCount": explain_plans.len(),
    }))
}

fn quick_diagnosis_manifest(
    trace_id: &str,
    site_name: &str,
    system: &str,
    database_type: &str,
    gateway_prefix: &str,
    logs: &[diag_core::models::LogEntry],
    sql_traces: &[diag_core::models::SqlTrace],
    time_window: &diag_core::models::TimeWindow,
    now: &DateTime<Utc>,
) -> diag_core::models::DiagnosisManifest {
    let mut services: Vec<String> = logs
        .iter()
        .map(|log| log.service.clone())
        .chain(sql_traces.iter().map(|trace| trace.service.clone()))
        .filter(|service| !service.is_empty())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    services.sort();

    diag_core::models::DiagnosisManifest {
        diagnosis_id: format!("quick-diag-{}-{}", trace_id, now.format("%Y%m%d-%H%M%S")),
        site: site_name.to_string(),
        system: system.to_string(),
        created_at: now.to_rfc3339(),
        page_url: "quick".to_string(),
        request_count: 0,
        services,
        trace_ids: vec![trace_id.to_string()],
        database_type: database_type.to_string(),
        privacy_level: "MASKED".to_string(),
        collector_version: env!("CARGO_PKG_VERSION").to_string(),
        collection_mode: Some("historical".to_string()),
        log_source: Some("elk".to_string()),
        gateway_prefix: Some(gateway_prefix.to_string()),
        keywords: None,
        time_range: Some(time_window.clone()),
    }
}

fn quick_diagnosis_report(
    now: &DateTime<Utc>,
    log_source: &str,
    log_count: usize,
    sql_trace_count: usize,
    explain_plan_count: usize,
    errors: Vec<String>,
) -> diag_core::models::CollectionReport {
    diag_core::models::CollectionReport {
        collected_at: now.to_rfc3339(),
        log_source: log_source.to_string(),
        log_count,
        sql_trace_count,
        explain_plan_count,
        skipped_services: Vec::new(),
        errors,
    }
}

fn quick_diagnosis_time_window(
    logs: &[diag_core::models::LogEntry],
) -> diag_core::models::TimeWindow {
    let parsed: Vec<DateTime<FixedOffset>> = logs
        .iter()
        .filter_map(|entry| entry.time.as_deref())
        .filter_map(|time| DateTime::parse_from_rfc3339(time).ok())
        .collect();

    if parsed.is_empty() {
        return diag_core::models::TimeWindow {
            start: String::new(),
            end: String::new(),
        };
    }

    let min_ts = parsed.iter().min().cloned().unwrap();
    let max_ts = parsed.iter().max().cloned().unwrap();
    diag_core::models::TimeWindow {
        start: (min_ts - Duration::minutes(5)).to_rfc3339(),
        end: (max_ts + Duration::minutes(5)).to_rfc3339(),
    }
}

/// 打开指定输出文件所在的目录
#[tauri::command]
pub fn open_output_dir(path: String) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    let dir = if p.is_dir() {
        p
    } else {
        p.parent().unwrap_or(p)
    };

    tracing::info!("正在打开输出目录: {:?}", dir);

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use diag_core::config::{
        CollectorSettings, DatabaseConfig, ElkConfig, GatewayConfig, PrivacyConfig, ServiceConfig,
        SiteConfig, SshConfig,
    };
    use diag_core::models::{LogEntry, SqlTrace, TimeWindow};

    fn mock_collector_config() -> CollectorConfig {
        CollectorConfig {
            site: SiteConfig {
                name: "test-site".into(),
                system: "pcm".into(),
            },
            gateway: GatewayConfig {
                prefix: "/gateway".into(),
            },
            services: vec![ServiceConfig {
                name: "svc-a".into(),
                display: "Svc A".into(),
                hosts: vec!["127.0.0.1".into()],
                log_dir: "/var/log".into(),
                log_pattern: "*.log".into(),
            }],
            ssh: SshConfig {
                port: 22,
                username: "ops".into(),
                auth_type: "password".into(),
                private_key: None,
                password: Some("pass".into()),
            },
            database: DatabaseConfig {
                db_type: "postgresql".into(),
                host: "127.0.0.1".into(),
                port: 5432,
                username: "readonly".into(),
                password: "dbpass".into(),
                database: "diag".into(),
                schemas: vec![],
            },
            privacy: PrivacyConfig {
                mask_query_values: true,
                allowed_query_keys: vec!["pageNum".into()],
            },
            collector: CollectorSettings {
                time_window_minutes: 30,
                max_log_lines: 200,
                output_dir: "./diagnosis-output".into(),
            },
            elk: Some(ElkConfig::default()),
            nacos: None,
            schedule: None,
        }
    }

    #[test]
    fn test_quick_diagnosis_manifest_preserves_historical_metadata() {
        let config = mock_collector_config();
        let logs = vec![
            LogEntry {
                time: Some("2026-06-23T10:00:00+08:00".into()),
                level: "INFO".into(),
                service: "svc-b".into(),
                trace_id: Some("trace-1".into()),
                thread: None,
                class: None,
                method: None,
                message: "ok".into(),
                exception: None,
                stack_trace: None,
                raw: "ok".into(),
            },
            LogEntry {
                time: Some("2026-06-23T10:01:00+08:00".into()),
                level: "ERROR".into(),
                service: "svc-a".into(),
                trace_id: Some("trace-1".into()),
                thread: None,
                class: None,
                method: None,
                message: "boom".into(),
                exception: None,
                stack_trace: None,
                raw: "boom".into(),
            },
        ];
        let sql_traces = vec![SqlTrace {
            trace_id: "trace-1".into(),
            service: "svc-c".into(),
            sql: "select 1".into(),
            sql_fingerprint: "select 1".into(),
            duration_ms: None,
            tables: vec![],
            timestamp: None,
            parameters: None,
        }];
        let time_window = TimeWindow {
            start: "2026-06-23T09:55:00+08:00".into(),
            end: "2026-06-23T10:05:00+08:00".into(),
        };
        let now = Utc.with_ymd_and_hms(2026, 6, 23, 14, 37, 27).unwrap();

        let manifest = quick_diagnosis_manifest(
            "trace-1",
            &config.site.name,
            &config.site.system,
            &config.database.db_type,
            &config.gateway.prefix,
            &logs,
            &sql_traces,
            &time_window,
            &now,
        );

        assert!(manifest.diagnosis_id.starts_with("quick-diag-trace-1-"));
        assert_eq!(manifest.collection_mode.as_deref(), Some("historical"));
        assert_eq!(manifest.page_url, "quick");
        assert_eq!(manifest.request_count, 0);
        assert_eq!(manifest.trace_ids, vec!["trace-1".to_string()]);
        assert_eq!(manifest.site, "test-site");
        assert_eq!(manifest.system, "pcm");
        assert_eq!(manifest.database_type, "postgresql");
        assert_eq!(manifest.log_source.as_deref(), Some("elk"));
        assert_eq!(manifest.gateway_prefix.as_deref(), Some("/gateway"));
        assert_eq!(
            manifest.time_range.as_ref().map(|w| (&w.start, &w.end)),
            Some((&time_window.start, &time_window.end))
        );
        assert_eq!(
            manifest.services,
            vec![
                "svc-a".to_string(),
                "svc-b".to_string(),
                "svc-c".to_string()
            ]
        );
    }
}
