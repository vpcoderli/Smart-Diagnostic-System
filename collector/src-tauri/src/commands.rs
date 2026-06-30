use chrono::{DateTime, Duration, FixedOffset, Utc};
use diag_core::config::CollectorConfig;
use diag_core::models::{CapturedPage, LogEntry, ResolvedUrl};
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

/// 日志源 × 诊断模式 的支持矩阵（唯一事实来源）。
///
/// - `realtime`：抓包后按 traceId 取日志，三种源都支持（ELK 还可降级 SSH）。
/// - `history` / `quick`：依赖 ELK/ES 的全文与 traceId 检索，SSH 暂不支持。
/// - `scheduler`：按关键词轮询，目前仅 ELK。
pub fn source_supports_mode(source: &str, mode: &str) -> bool {
    match mode {
        "realtime" => matches!(source, "elk" | "es" | "ssh"),
        "history" | "quick" => matches!(source, "elk" | "es"),
        "scheduler" => source == "elk",
        _ => false,
    }
}

const DIAG_DATA_START_MARKER: &str = "__DIAG_DATA_START__";
const DIAG_DATA_END_MARKER: &str = "__DIAG_DATA_END__";

fn extract_diag_data_from_title(title: &str) -> Option<String> {
    let start = title.find(DIAG_DATA_START_MARKER)?;
    let data_start = start + DIAG_DATA_START_MARKER.len();
    let rest = &title[data_start..];
    let end = rest.find(DIAG_DATA_END_MARKER)?;
    if end == 0 {
        None
    } else {
        Some(rest[..end].to_string())
    }
}

fn extract_diag_count_from_title(title: &str) -> Option<usize> {
    let start = title.find("[DIAG:")?;
    let rest = &title[start + 6..];
    let end = rest.find(']')?;
    rest[..end].parse::<usize>().ok()
}

fn read_collected_diag_data(
    captured_store: &Arc<crate::webview_capture::CapturedDataStore>,
    window: &tauri::WebviewWindow,
) -> Option<String> {
    if let Some(json) = captured_store.data.lock().unwrap().clone() {
        return Some(json);
    }

    window
        .title()
        .ok()
        .and_then(|title| extract_diag_data_from_title(&title))
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
            es: None,
            schedule: None,
            log_source: None,
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
            es: None,
            schedule: None,
            log_source: None,
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
            es: None,
            schedule: None,
            log_source: None,
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
pub async fn open_diag_browser(
    url: String,
    app: tauri::AppHandle,
    captured_store: State<'_, std::sync::Arc<crate::webview_capture::CapturedDataStore>>,
    count: State<'_, std::sync::Arc<std::sync::Mutex<usize>>>,
) -> Result<String, String> {
    // 清除上一次的捕获数据
    *captured_store.data.lock().unwrap() = None;
    *count.lock().unwrap() = 0;

    crate::webview_capture::open_diagnostic_window(&app, &url)?;
    Ok(format!("诊断浏览器已打开: {}", url))
}

/// 从诊断浏览器收集捕获的请求数据
/// 外部医院页面在 Windows WebView2 中不能用 XHR/fetch 访问 diag://，因此优先读取标题中转数据。
#[tauri::command]
pub async fn collect_diag_data(
    app: tauri::AppHandle,
    captured_store: State<'_, std::sync::Arc<crate::webview_capture::CapturedDataStore>>,
) -> Result<String, String> {
    use tauri::Manager;

    // 清除旧数据
    *captured_store.data.lock().unwrap() = None;

    let window = app
        .get_webview_window("diagnostic")
        .ok_or("诊断窗口未打开")?;

    // 触发诊断窗口发送数据（Windows 外部页通过标题中转，旧路径仍可写入 captured_store）
    crate::webview_capture::trigger_data_collection(&app)?;

    // 等待数据到达（最多等 3 秒）
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(json) = read_collected_diag_data(captured_store.inner(), &window) {
            tracing::info!("已收集诊断数据: {} bytes", json.len());
            return Ok(json);
        }
    }

    // Fallback 1: 再次触发
    tracing::warn!("首次标题中转超时，重试触发诊断数据回传...");

    let _ = window.eval(
        "try { window.__sendDiagData(); } catch(e) { console.error('sendDiagData failed:', e); }",
    );

    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(json) = read_collected_diag_data(captured_store.inner(), &window) {
            tracing::info!("已收集诊断数据(重试): {} bytes", json.len());
            return Ok(json);
        }
    }

    // Fallback 2: 直接写标题，避开 custom scheme 的 CORS/协议限制。
    tracing::warn!("诊断脚本回传超时，尝试直接 title 中转...");
    let _ = window.eval(r#"
        (function() {
            try {
                var data = window.__getDiagData
                    ? window.__getDiagData()
                    : JSON.stringify({ pageUrl: location.href, requests: window.__diag_requests || [] });
                document.title = '__DIAG_DATA_START__' + data + '__DIAG_DATA_END__';
            } catch(e) { console.error('[Smart-Diag] title fallback failed:', e); }
        })();
    "#);

    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(json) = read_collected_diag_data(captured_store.inner(), &window) {
            tracing::info!("已收集诊断数据(title中转): {} bytes", json.len());
            return Ok(json);
        }
    }

    Err("采集数据超时。请确认诊断浏览器窗口仍在运行，目标页面加载完成后再点击收集。".to_string())
}

/// 获取当前捕获的请求计数（实时轮询用）
/// Windows 外部页通过标题回传计数；旧 custom protocol 计数器作为兜底。
#[tauri::command]
pub async fn get_capture_count(
    count: State<'_, std::sync::Arc<std::sync::Mutex<usize>>>,
    app: tauri::AppHandle,
) -> Result<usize, String> {
    use tauri::Manager;
    if let Some(window) = app.get_webview_window("diagnostic") {
        if let Ok(title) = window.title() {
            if let Some(n) = extract_diag_count_from_title(&title) {
                *count.lock().unwrap() = n;
                return Ok(n);
            }
        }
    }
    Ok(*count.lock().unwrap())
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
        let _ = window.eval(r#"
            (function() {
                try {
                    if (window.__resetDiagCapture) {
                        window.__resetDiagCapture();
                    } else {
                        window.__diag_requests = [];
                        window.__diag_frame_requests = {};
                        window.__diag_frame_pages = {};
                        window.__diag_page_url = location.href;
                        if (document && document.title) {
                            document.title = document.title
                                .replace(/__DIAG_DATA_START__[\s\S]*?__DIAG_DATA_END__/g, '')
                                .replace(/^\[DIAG:\d+\]\s*/, '');
                        }
                    }
                } catch(e) {}
            })();
        "#);
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

/// 打开诊断浏览器的开发者工具（调试用）
#[tauri::command]
pub fn open_diag_devtools(app: tauri::AppHandle) -> Result<(), String> {
    crate::webview_capture::open_diagnostic_devtools(&app)
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
            trace_id: e.field_trace_id.clone().unwrap_or_else(|| "traceId".into()),
            message: e.field_message.clone().unwrap_or_else(|| "message".into()),
            exception: "exception".into(),
            stack_trace: "stackTrace".into(),
            thread: "thread".into(),
        },
    })
}

/// 实时模式：从 manifest 最新 ES 配置构建 EsConfig，保证字段映射与快速诊断一致
fn build_es_config_from_manifest(
    manifest: &crate::deployment::DeploymentManifest,
) -> Option<diag_core::config::EsConfig> {
    use diag_core::config::{EsConfig, FieldMapping};
    manifest.es.as_ref().map(|e| EsConfig {
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
            trace_id: e.field_trace_id.clone().unwrap_or_else(|| "traceId".into()),
            message: e.field_message.clone().unwrap_or_else(|| "message".into()), // ES 默认字段
            exception: "exception".into(),
            stack_trace: "stackTrace".into(),
            thread: "thread".into(),
        },
    })
}

fn has_ssh_fallback_config(config: &CollectorConfig) -> bool {
    !config.services.is_empty()
        && !config.ssh.username.trim().is_empty()
        && (config
            .ssh
            .password
            .as_deref()
            .map(|password| !password.trim().is_empty())
            .unwrap_or(false)
            || config
                .ssh
                .private_key
                .as_deref()
                .map(|private_key| !private_key.trim().is_empty())
                .unwrap_or(false))
}

fn log_matches_text_terms(log: &LogEntry, terms: &[&str]) -> bool {
    let fields = [
        log.message.to_lowercase(),
        log.raw.to_lowercase(),
        log.exception
            .as_deref()
            .unwrap_or_default()
            .to_lowercase(),
        log.stack_trace
            .as_deref()
            .unwrap_or_default()
            .to_lowercase(),
    ];

    terms.iter()
        .map(|term| term.trim().to_lowercase())
        .filter(|term| !term.is_empty())
        .all(|term| fields.iter().any(|field| field.contains(&term)))
}

fn filter_logs_by_text_terms(logs: Vec<LogEntry>, terms: &[&str]) -> Vec<LogEntry> {
    if terms.iter().all(|term| term.trim().is_empty()) {
        return logs;
    }

    logs.into_iter()
        .filter(|log| log_matches_text_terms(log, terms))
        .collect()
}

/// 启动完整诊断流程（实时模式）
#[tauri::command]
pub async fn start_diagnosis(
    captured_json: String,
    log_source: Option<String>,
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

    // 从 manifest 读取最新 ELK 和 ES 配置，分别覆盖 config
    {
        let manifest = state.manifest.lock().unwrap();
        if let Some(ref m) = *manifest {
            if let Some(elk_cfg) = build_elk_config_from_manifest(m) {
                config.elk = Some(elk_cfg);
            }
            if let Some(es_cfg) = build_es_config_from_manifest(m) {
                config.es = Some(es_cfg);
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

    let source = log_source.unwrap_or_else(|| "elk".to_string());
    tracing::info!(
        "启动诊断: 页面={}, 请求数={}, 站点={}, 日志源={}",
        captured.page_url,
        captured.requests.len(),
        config.site.name,
        source
    );

    let log_collector: Box<dyn diag_core::collector_trait::LogCollector> = match source.as_str() {
        "es" => {
            if let Some(es_cfg) = &config.es {
                Box::new(crate::es_collector::EsCollector::new(es_cfg.clone()).await
                    .map_err(|e| format!("ES 初始化失败: {}", e))?)
            } else {
                return Err("未配置 ES 直接连接信息".to_string());
            }
        }
        "elk" => {
            if let Some(elk_cfg) = &config.elk {
                match crate::elk_collector::ElkCollector::new(elk_cfg.clone()).await {
                    Ok(elk) => Box::new(elk),
                    Err(e) => {
                        if has_ssh_fallback_config(&config) {
                            tracing::warn!("ELK 不可用 ({}), 降级 SSH", e);
                            Box::new(SshLogCollector::new(
                                config.ssh.clone(),
                                config.services.clone(),
                                config.collector.max_log_lines,
                            ))
                        } else {
                            return Err(format!("ELK 初始化失败，且未配置可用 SSH 降级: {}", e));
                        }
                    }
                }
            } else if has_ssh_fallback_config(&config) {
                Box::new(SshLogCollector::new(
                    config.ssh.clone(),
                    config.services.clone(),
                    config.collector.max_log_lines,
                ))
            } else {
                return Err("未配置 ELK，且未配置可用 SSH 日志采集".to_string());
            }
        }
        "ssh" | _ => {
            if has_ssh_fallback_config(&config) {
                Box::new(SshLogCollector::new(
                    config.ssh.clone(),
                    config.services.clone(),
                    config.collector.max_log_lines,
                ))
            } else {
                return Err("未配置可用 SSH 日志采集".to_string());
            }
        }
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
// ES 相关命令
// ═══════════════════════════════════════

/// 设置 ES 配置
#[tauri::command]
pub fn set_es_config(
    es: crate::deployment::EsDeployment,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let mut manifest = state.manifest.lock().unwrap();
    if let Some(ref mut m) = *manifest {
        m.es = Some(es);
        let config = crate::deployment::manifest_to_collector_config(m);
        *state.config.lock().unwrap() = Some(config);
        Ok("ES 配置已设置".to_string())
    } else {
        // 保留已有 ELK 配置，新建时 elk 为空
        let manifest_new = crate::deployment::DeploymentManifest {
            site_name: String::new(),
            system: "pcm".to_string(),
            gateway_prefix: "/gateway".to_string(),
            services: Vec::new(),
            databases: Vec::new(),
            elk: None, // 新建时 ELK 为空，不影响已存在 manifest 分支
            es: Some(es),
            schedule: None,
            log_source: None,
        };
        let config = crate::deployment::manifest_to_collector_config(&manifest_new);
        *manifest = Some(manifest_new);
        *state.config.lock().unwrap() = Some(config);
        Ok("ES 配置已设置（新建配置）".to_string())
    }
}

/// 测试 ES 直接连接
#[tauri::command]
pub async fn test_es_connection(
    address: String,
    index_pattern: String,
    username: Option<String>,
    password: Option<String>,
) -> Result<serde_json::Value, String> {
    use diag_core::config::{EsConfig, FieldMapping};

    let config = EsConfig {
        address,
        index_pattern,
        username,
        password,
        timeout_secs: 10,
        max_hits_per_trace: 1,
        field_mapping: FieldMapping::default(),
    };

    match crate::es_collector::EsCollector::new(config).await {
        Ok(collector) => {
            match collector.get_es_version().await {
                Ok(version) => Ok(serde_json::json!({
                    "success": true,
                    "message": "ES 连接成功",
                    "esVersion": version,
                })),
                Err(e) => Err(format!("ES 校验请求失败: {}", e)),
            }
        }
        Err(e) => Err(format!("ES 客户端初始化失败: {}", e)),
    }
}

// ═══════════════════════════════════════
// 日志来源选择持久化
// ═══════════════════════════════════════

/// 持久化保存当前选择的日志来源（elk / es / ssh）
#[tauri::command]
pub fn set_log_source(
    log_source: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let mut manifest = state.manifest.lock().unwrap();
    if let Some(ref mut m) = *manifest {
        m.log_source = Some(log_source.clone());
        Ok(format!("日志来源已设置为 {}", log_source))
    } else {
        Err("尚未初始化配置，请先完成第一步配置".to_string())
    }
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
        // 同步更新 state.config，与 set_es_config 对称
        let config = crate::deployment::manifest_to_collector_config(m);
        *state.config.lock().unwrap() = Some(config);
        Ok("ELK 配置已设置".to_string())
    } else {
        // 新建 manifest 时保留 es: None（没有旧的 manifest 就没有 ES 配置）
        let manifest_new = crate::deployment::DeploymentManifest {
            site_name: String::new(),
            system: "pcm".to_string(),
            gateway_prefix: "/gateway".to_string(),
            services: Vec::new(),
            databases: Vec::new(),
            elk: Some(elk),
            es: None,
            schedule: None,
            log_source: None,
        };
        let config = crate::deployment::manifest_to_collector_config(&manifest_new);
        *manifest = Some(manifest_new);
        *state.config.lock().unwrap() = Some(config);
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

/// 启发式推断日志字段映射
fn guess_fields(mapping_json: &serde_json::Value) -> diag_core::config::FieldMapping {
    let mut mapping = diag_core::config::FieldMapping::default();

    if let Some(obj) = mapping_json.as_object() {
        for (_index, val) in obj {
            if let Some(properties) = val.get("mappings").and_then(|m| m.get("properties")).and_then(|p| p.as_object()) {
                for (field_name, field_def) in properties {
                    let field_type = field_def.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    let name_lower = field_name.to_lowercase();

                    // 1. Timestamp (type is date)
                    if field_type == "date" {
                        if name_lower.contains("time") || name_lower.contains("date") || field_name == "@timestamp" {
                            mapping.timestamp = field_name.clone();
                        }
                    }

                    // 2. Level
                    if name_lower == "level" || name_lower == "loglevel" || name_lower == "log.level" || name_lower == "severity" {
                        mapping.level = field_name.clone();
                    }

                    // 3. TraceId
                    if name_lower == "traceid" || name_lower == "trace_id" || name_lower == "x0" || name_lower == "tid" {
                        mapping.trace_id = field_name.clone();
                    }

                    // 4. Service
                    if name_lower == "servicename" || name_lower == "service_name" || name_lower == "service" || name_lower == "app" || name_lower == "appname" {
                        if field_type == "keyword" {
                            mapping.service = field_name.clone();
                        } else if field_def.get("fields").and_then(|f| f.get("keyword")).is_some() {
                            mapping.service = format!("{}.keyword", field_name);
                        } else {
                            mapping.service = field_name.clone();
                        }
                    }

                    // 5. Message
                    if name_lower == "message" || name_lower == "msg" || name_lower == "content" || name_lower == "log_message" {
                        mapping.message = field_name.clone();
                    }
                }
            }
        }
    }

    mapping
}

/// 智能从 ES 探查日志配置（索引映射 + 活跃微服务）
#[tauri::command]
pub async fn discover_log_config_from_es(
    address: String,
    index_pattern: String,
    username: Option<String>,
    password: Option<String>,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    use elasticsearch::{Elasticsearch, SearchParts};
    use elasticsearch::indices::IndicesGetMappingParts;
    use elasticsearch::http::transport::{TransportBuilder, SingleNodeConnectionPool};
    use elasticsearch::auth::Credentials;
    use elasticsearch::cert::CertificateValidation;
    use url::Url;
    use serde_json::json;

    tracing::info!("开始从 ES 智能探查日志配置: address={}, index_pattern={}", address, index_pattern);

    let url_parsed = Url::parse(&address).map_err(|e| format!("无效的 ES 地址: {}", e))?;
    let conn_pool = SingleNodeConnectionPool::new(url_parsed);
    let mut builder = TransportBuilder::new(conn_pool);
    if let (Some(u), Some(p)) = (&username, &password) {
        if !u.trim().is_empty() {
            builder = builder.auth(Credentials::Basic(u.clone(), p.clone()));
        }
    }
    builder = builder.cert_validation(CertificateValidation::None);
    let transport = builder.build().map_err(|e| format!("构建 ES Transport 失败: {}", e))?;
    let client = Elasticsearch::new(transport);

    // 1. 获取 Mapping
    tracing::info!("发送 ES Mapping 请求...");
    let mapping_resp = client
        .indices()
        .get_mapping(IndicesGetMappingParts::Index(&[&index_pattern]))
        .send()
        .await
        .map_err(|e| format!("获取 ES Mapping 失败: {}", e))?;

    let mapping_body = mapping_resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("解析 Mapping JSON 失败: {}", e))?;

    tracing::info!("ES Mapping 返回成功，开始推断字段映射...");
    let guessed_mapping = guess_fields(&mapping_body);
    tracing::info!("推断字段映射结果: {:?}", guessed_mapping);

    // 2. 利用推论出的服务名（如果有），做 Terms 聚合获取活跃服务名列表
    let service_field = guessed_mapping.service.clone();
    tracing::info!("使用服务名域 '{}' 进行 Terms 聚合查询...", service_field);

    let agg_query = json!({
        "size": 0,
        "aggs": {
            "unique_services": {
                "terms": {
                    "field": service_field,
                    "size": 100
                }
            }
        }
    });

    let search_resp = client
        .search(SearchParts::Index(&[&index_pattern]))
        .body(agg_query)
        .send()
        .await
        .map_err(|e| format!("ES 聚合查询服务名失败: {}", e))?;

    let search_body = search_resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("解析聚合响应 JSON 失败: {}", e))?;

    let mut services = Vec::new();
    if let Some(buckets) = search_body.get("aggregations")
        .and_then(|a| a.get("unique_services"))
        .and_then(|u| u.get("buckets"))
        .and_then(|b| b.as_array()) {
        for bucket in buckets {
            if let Some(key) = bucket.get("key").and_then(|k| k.as_str()) {
                services.push(key.to_string());
            }
        }
    }

    tracing::info!("从 ES 探查成功，共发现服务 {} 个: {:?}", services.len(), services);

    // 3. 将发现的服务写入 Manifest，以便后续流程无需手动导入 CSV
    let mut manifest = state.manifest.lock().unwrap();
    if let Some(ref mut m) = *manifest {
        // 创建或更新微服务配置
        let mut new_services = m.services.clone();
        for svc_name in &services {
            if !new_services.iter().any(|s| s.project_name == *svc_name) {
                new_services.push(crate::deployment::ServiceDeployment {
                    project_name: svc_name.clone(),
                    server_ip: "elk".to_string(), // ES 模式下设为占位符
                    ssh_username: String::new(),
                    ssh_password: String::new(),
                    ssh_port: 22,
                    log_path: String::new(),
                    log_pattern: String::new(),
                });
            }
        }
        m.services = new_services;
    } else {
        // 新建 manifest 并填入服务
        let s_deployments = services.iter().map(|svc_name| crate::deployment::ServiceDeployment {
            project_name: svc_name.clone(),
            server_ip: "elk".to_string(),
            ssh_username: String::new(),
            ssh_password: String::new(),
            ssh_port: 22,
            log_path: String::new(),
            log_pattern: String::new(),
        }).collect();
        *manifest = Some(crate::deployment::DeploymentManifest {
            site_name: String::new(),
            system: "pcm".to_string(),
            gateway_prefix: "/gateway".to_string(),
            services: s_deployments,
            databases: Vec::new(),
            elk: None,
            es: None,
            schedule: None,
            log_source: None,
        });
    }

    // 刷新 AppState::config
    if let Some(ref m) = *manifest {
        let config = crate::deployment::manifest_to_collector_config(m);
        *state.config.lock().unwrap() = Some(config);
    }

    Ok(json!({
        "success": true,
        "fieldMapping": {
            "timestamp": guessed_mapping.timestamp,
            "level": guessed_mapping.level,
            "traceId": guessed_mapping.trace_id,
            "service": guessed_mapping.service,
            "message": guessed_mapping.message,
        },
        "services": services,
    }))
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

    // 定时巡检按关键词轮询 ELK，目前仅支持 ELK 日志源。
    if config.elk.is_none() {
        return Err("定时巡检目前仅支持 ELK 日志源，请先在 Configure 页面配置 ELK 地址".to_string());
    }
    if config.schedule.is_none() {
        return Err("尚未配置定时巡检参数，请在第一步开启并填写巡检配置".to_string());
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

#[tauri::command]
pub async fn start_historical_diagnosis(
    keywords: Vec<String>,
    time_start: String,
    time_end: String,
    log_source: Option<String>,
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

    // 从 manifest 读取最新 ELK 和 ES 配置，与实时/快速模式保持一致
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
            if let Some(es_cfg) = build_es_config_from_manifest(m) {
                tracing::info!(
                    "[历史诊断] 从 manifest 刷新 ES 配置: index={}, trace_id_field={}",
                    es_cfg.index_pattern,
                    es_cfg.field_mapping.trace_id
                );
                config.es = Some(es_cfg);
            }
        }
    }

    resolve_output_dir(&app, &mut config);

    let source = log_source.unwrap_or_else(|| "elk".to_string());

    // 历史诊断依赖关键词全文检索，仅支持 ELK / ES；SSH 等其它源直接拒绝，避免落到 elk.unwrap() 崩溃。
    if !source_supports_mode(&source, "history") {
        return Err(format!(
            "历史诊断模式暂不支持「{}」日志源，请切换为 ELK 或 ES",
            source
        ));
    }
    if source == "elk" && config.elk.is_none() {
        return Err("历史模式需要 ELK 配置，请在 Configure 页面填写 ELK 地址".to_string());
    }
    if source == "es" && config.es.is_none() {
        return Err("历史模式需要 ES 配置，请在 Configure 页面填写 ES 地址".to_string());
    }

    let window = diag_core::models::TimeWindow {
        start: time_start.clone(),
        end: time_end.clone(),
    };

    // ── Step 1：连接 ELK/ES ──
    emit_step(
        &app,
        "elk-connect",
        &format!("正在连接 {}...", if source == "es" { "ES" } else { "ELK" }),
        "running",
    );

    let log_collector: Box<dyn diag_core::collector_trait::LogCollector> = match source.as_str() {
        "es" => {
            let es_config = config.es.as_ref().ok_or("历史模式需要 ES 配置")?.clone();
            let es_collector = crate::es_collector::EsCollector::new(es_config)
                .await
                .map_err(|e| {
                    emit_step(
                        &app,
                        "elk-connect",
                        &format!("ES 连接失败：{}", e),
                        "error",
                    );
                    format!("ES 连接失败: {}", e)
                })?;
            emit_step(
                &app,
                "elk-connect",
                "ES 连接成功",
                "done",
            );
            Box::new(es_collector)
        }
        _ => {
            let elk_config = config.elk.as_ref().ok_or("历史模式需要 ELK 配置")?.clone();
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
            Box::new(elk_collector)
        }
    };

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
        let query_detail = if text_keys.is_empty() {
            format!("traceId 精确查询：{}", tids.join(", "))
        } else {
            format!(
                "traceId 精确查询并过滤关键词：{}；关键词：{}",
                tids.join(", "),
                text_keys.join(" ")
            )
        };
        emit_step(
            &app,
            "elk-query",
            &query_detail,
            "running",
        );
        let trace_logs = log_collector
            .query_by_trace_ids(&tids, None, &window)
            .await
            .map_err(|e| {
                emit_step(
                    &app,
                    "elk-query",
                    &format!("traceId 查询失败：{}", e),
                    "error",
                );
                format!("查询失败: {}", e)
            })?;

        if text_keys.is_empty() {
            trace_logs
        } else {
            filter_logs_by_text_terms(trace_logs, &text_keys)
        }
    } else {
        // 普通关键词全文搜索
        let text_keywords: Vec<String> = text_keys.iter().map(|s| s.to_string()).collect();
        log_collector
            .query_by_keywords(&text_keywords, None, &window)
            .await
            .map_err(|e| {
                emit_step(&app, "elk-query", &format!("查询失败：{}", e), "error");
                format!("查询失败: {}", e)
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
            "按 {} 个 traceId 从 {} 采集完整链路日志...",
            trace_ids.len(),
            if source == "es" { "ES" } else { "ELK" }
        ),
        "running",
    );

    // DiagnosisRunner 内部会采集完整链路 + SQL + 打包
    // 在 run() 前先通知前端"采集中"
    let runner = crate::diagnosis::DiagnosisRunner::new_historical_with_window(
        config.clone(),
        log_collector,
        trace_ids.clone(),
        window.clone(),
    );

    emit_step(
        &app,
        "collect-logs",
        &format!(
            "traceId 关联日志采集中（{} 并发查询）...",
            if source == "es" { "ES" } else { "ELK" }
        ),
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

/// 快速诊断：输入单个 traceId，直接从 ELK/ES 查询日志并输出 TXT 格式 ZIP
#[tauri::command]
pub async fn start_quick_diagnosis(
    trace_id: String,
    field_trace_id: Option<String>,
    field_message: Option<String>,
    index_pattern: Option<String>,
    output_dir: Option<String>,
    log_source: Option<String>,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    use diag_core::config::{FieldMapping};

    // 设置 panic hook 以便在崩溃时记录更多信息
    tracing::info!(
        "快速诊断启动: trace_id={}, field_trace_id={:?}, field_message={:?}, log_source={:?}",
        trace_id,
        field_trace_id,
        field_message,
        log_source
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

    let source = log_source.unwrap_or_else(|| "elk".to_string());

    // 快速诊断按 traceId 检索，仅支持 ELK / ES；SSH 等其它源给出明确提示。
    if !source_supports_mode(&source, "quick") {
        return Err(format!(
            "快速诊断模式暂不支持「{}」日志源，请切换为 ELK 或 ES",
            source
        ));
    }

    let log_collector: Box<dyn diag_core::collector_trait::LogCollector> = match source.as_str() {
        "es" => {
            let es_deployment = {
                let manifest = state.manifest.lock().unwrap();
                let manifest = manifest.as_ref().ok_or("请先配置 ES 直接模式配置信息")?;
                manifest
                    .es
                    .as_ref()
                    .ok_or("快速诊断需要 ES 配置")?
                    .clone()
            };
            let idx_pattern = index_pattern.unwrap_or_else(|| es_deployment.index_pattern.clone());
            let field_tid = field_trace_id
                .filter(|s| !s.trim().is_empty())
                .or_else(|| es_deployment.field_trace_id.clone())
                .unwrap_or_else(|| "traceId".to_string());
            let field_msg = field_message
                .filter(|s| !s.trim().is_empty())
                .or_else(|| es_deployment.field_message.clone())
                .unwrap_or_else(|| "message".to_string());

            let es_config = diag_core::config::EsConfig {
                address: es_deployment.address.clone(),
                index_pattern: idx_pattern.clone(),
                username: es_deployment.username.clone(),
                password: es_deployment.password.clone(),
                timeout_secs: es_deployment.timeout_secs.unwrap_or(30),
                max_hits_per_trace: es_deployment.max_hits_per_trace.unwrap_or(2000),
                field_mapping: FieldMapping {
                    timestamp: es_deployment.field_timestamp.clone().unwrap_or_else(|| "@timestamp".into()),
                    level: es_deployment.field_level.clone().unwrap_or_else(|| "level".into()),
                    trace_id: field_tid.clone(),
                    service: es_deployment.field_service.clone().unwrap_or_else(|| "serviceName".into()),
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
                    "连接 ES：{}，索引={}，traceId 字段={}, msg 字段={}",
                    es_config.address, idx_pattern, field_tid, field_msg
                ),
                "running",
            );

            let es_collector = crate::es_collector::EsCollector::new(es_config).await
                .map_err(|e| {
                    emit_step(
                        &app,
                        "elk-connect",
                        &format!("ES 连接失败：{}", e),
                        "error",
                    );
                    format!("ES 连接失败: {}", e)
                })?;

            emit_step(
                &app,
                "elk-connect",
                "ES 连接成功",
                "done",
            );
            Box::new(es_collector)
        }
        _ => {
            let elk_deployment = {
                let manifest = state.manifest.lock().unwrap();
                let manifest = manifest.as_ref().ok_or("请先配置 ELK 信息")?;
                manifest
                    .elk
                    .as_ref()
                    .ok_or("快速诊断需要 ELK 配置")?
                    .clone()
            };
            let idx_pattern = index_pattern.unwrap_or_else(|| elk_deployment.index_pattern.clone());
            let field_tid = field_trace_id
                .filter(|s| !s.trim().is_empty())
                .or_else(|| elk_deployment.field_trace_id.clone())
                .unwrap_or_else(|| "traceId".to_string());
            let field_msg = field_message
                .filter(|s| !s.trim().is_empty())
                .or_else(|| elk_deployment.field_message.clone())
                .unwrap_or_else(|| "message".to_string());

            let elk_config = diag_core::config::ElkConfig {
                address: elk_deployment.address.clone(),
                index_pattern: idx_pattern.clone(),
                username: elk_deployment.username.clone(),
                password: elk_deployment.password.clone(),
                timeout_secs: elk_deployment.timeout_secs.unwrap_or(30),
                max_hits_per_trace: elk_deployment.max_hits_per_trace.unwrap_or(2000),
                field_mapping: FieldMapping {
                    timestamp: elk_deployment.field_timestamp.clone().unwrap_or_else(|| "@timestamp".into()),
                    level: elk_deployment.field_level.clone().unwrap_or_else(|| "level".into()),
                    trace_id: field_tid.clone(),
                    service: elk_deployment.field_service.clone().unwrap_or_else(|| "serviceName".into()),
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

            let elk_collector = crate::elk_collector::ElkCollector::new(elk_config).await
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
            Box::new(elk_collector)
        }
    };

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

    let logs = log_collector
        .query_by_trace_ids(&[trace_id.clone()], None, &empty_window)
        .await
        .map_err(|e| {
            emit_step(&app, "elk-query", &format!("查询失败：{}", e), "error");
            format!("查询失败: {}", e)
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
    use super::{extract_diag_count_from_title, extract_diag_data_from_title, source_supports_mode};

    #[test]
    fn realtime_supports_all_three_sources() {
        for src in ["elk", "es", "ssh"] {
            assert!(
                source_supports_mode(src, "realtime"),
                "realtime 应支持 {src}"
            );
        }
    }

    #[test]
    fn history_and_quick_support_only_elk_and_es() {
        for mode in ["history", "quick"] {
            assert!(source_supports_mode("elk", mode));
            assert!(source_supports_mode("es", mode));
            assert!(
                !source_supports_mode("ssh", mode),
                "{mode} 不应支持 ssh（会落到 elk.unwrap 崩溃）"
            );
        }
    }

    #[test]
    fn scheduler_supports_only_elk() {
        assert!(source_supports_mode("elk", "scheduler"));
        assert!(!source_supports_mode("es", "scheduler"));
        assert!(!source_supports_mode("ssh", "scheduler"));
    }

    #[test]
    fn unknown_source_or_mode_is_unsupported() {
        assert!(!source_supports_mode("kafka", "realtime"));
        assert!(!source_supports_mode("elk", "teleport"));
        assert!(!source_supports_mode("", ""));
    }

    #[test]
    fn extract_diag_data_from_title_reads_marker_payload() {
        let payload = r#"{"pageUrl":"http://host/app","requests":[{"url":"/api/patient"}]}"#;
        let title = format!("__DIAG_DATA_START__{}__DIAG_DATA_END__", payload);

        assert_eq!(extract_diag_data_from_title(&title), Some(payload.to_string()));
    }

    #[test]
    fn extract_diag_data_from_title_ignores_missing_or_empty_payload() {
        assert_eq!(extract_diag_data_from_title("[DIAG:3] 页面"), None);
        assert_eq!(
            extract_diag_data_from_title("__DIAG_DATA_START____DIAG_DATA_END__"),
            None
        );
        assert_eq!(
            extract_diag_data_from_title("__DIAG_DATA_START__{}"),
            None
        );
    }

    #[test]
    fn extract_diag_count_from_title_reads_latest_count() {
        assert_eq!(extract_diag_count_from_title("[DIAG:12] 患者管理"), Some(12));
        assert_eq!(extract_diag_count_from_title("诊断浏览器 [DIAG:0]"), Some(0));
        assert_eq!(extract_diag_count_from_title("诊断浏览器"), None);
        assert_eq!(extract_diag_count_from_title("[DIAG:abc] 页面"), None);
    }
}
