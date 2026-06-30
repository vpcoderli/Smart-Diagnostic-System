mod cleanup;
mod commands;
mod config_store;
mod db_collector;
mod dedup_cache;
mod deployment;
mod diagnosis;
mod elk_collector;
mod es_collector;
mod explain_collector;
mod nacos_discovery;
mod scheduler;
mod sql_extractor;
mod ssh_collector;
mod ssh_log_collector;
mod validator;
mod webview_capture;

use commands::*;
use std::sync::Mutex;
use webview_capture::CapturedDataStore;

fn query_param(uri: &str, name: &str) -> Option<String> {
    let query = uri.split_once('?')?.1.split('#').next().unwrap_or("");
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

fn decode_diag_collect_payload(uri: &str, body: &[u8]) -> String {
    let payload = String::from_utf8_lossy(body).into_owned();
    let trimmed = payload.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return payload;
    }

    if let Some((_, value)) =
        url::form_urlencoded::parse(payload.as_bytes()).find(|(key, _)| key == "data")
    {
        return value.into_owned();
    }

    query_param(uri, "data").unwrap_or(payload)
}

#[cfg(test)]
mod tests {
    #[test]
    fn decode_diag_collect_payload_reads_query_data_when_body_is_empty() {
        let payload = super::decode_diag_collect_payload(
            "diag://collect?data=%7B%22pageUrl%22%3A%22http%3A%2F%2Fhost%22%2C%22requests%22%3A%5B%5D%7D",
            &[],
        );

        assert_eq!(payload, r#"{"pageUrl":"http://host","requests":[]}"#);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    tracing::info!("Smart Diagnostic Collector 启动中...");

    // 捕获数据存储 — 诊断窗口通过 custom protocol 将数据写入此处
    let captured_store = std::sync::Arc::new(CapturedDataStore::default());
    let captured_store_for_protocol = captured_store.clone();

    // 请求计数存储 — 实时更新前端请求计数
    let request_count: std::sync::Arc<Mutex<usize>> = std::sync::Arc::new(Mutex::new(0));
    let count_for_protocol = request_count.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            manifest: Mutex::new(None),
            config: Mutex::new(None),
            validated: Mutex::new(false),
            scheduler_status: std::sync::Arc::new(std::sync::Mutex::new(
                crate::scheduler::SchedulerStatus::default(),
            )),
            scheduler_handle: Mutex::new(None),
        })
        .manage(captured_store)
        .manage(request_count)
        // ── setup 钩子：应用初始化后用正确的 app_data_dir 加载已保存配置 ──
        .setup(|app| {
            use tauri::Manager;
            if let Ok(data_dir) = app.path().app_data_dir() {
                match crate::config_store::load_config(&data_dir) {
                    Ok(manifest) => {
                        tracing::info!("启动时加载已保存配置: 站点={}", manifest.site_name);
                        let config = crate::deployment::manifest_to_collector_config(&manifest);
                        let state = app.state::<AppState>();
                        *state.manifest.lock().unwrap() = Some(manifest);
                        *state.config.lock().unwrap() = Some(config);
                        // validated 仍为 false，每次启动需重新校验
                    }
                    Err(e) => {
                        tracing::debug!("无已保存配置（首次启动或已清空）: {}", e);
                    }
                }
            }
            Ok(())
        })
        // ═══ custom protocol: 诊断窗口 → Rust 数据通道 ═══
        .register_asynchronous_uri_scheme_protocol("diag", move |_ctx, request, responder| {
            let uri = request.uri().to_string();
            let body = request.body().to_vec();

            tracing::debug!("diag:// 协议收到请求: uri={}, body_len={}", uri, body.len());

            if uri.contains("redirect-target") {
                let body = crate::webview_capture::diagnostic_redirect_target_json();
                let response = http::Response::builder()
                    .status(200)
                    .header("Content-Type", "application/json; charset=utf-8")
                    .header("Access-Control-Allow-Origin", "*")
                    .header("Access-Control-Allow-Methods", "GET, OPTIONS")
                    .header("Access-Control-Allow-Headers", "Content-Type")
                    .body(body.into_bytes())
                    .unwrap();
                responder.respond(response);
            } else if uri.contains("collect-chunk") {
                let id = query_param(&uri, "id").unwrap_or_default();
                let index = query_param(&uri, "index")
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(usize::MAX);
                let total = query_param(&uri, "total")
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                let data = query_param(&uri, "data").unwrap_or_default();

                match captured_store_for_protocol.store_chunk(id, index, total, data) {
                    Ok(Some(json)) => {
                        tracing::info!("收到诊断分片数据并组装完成: {} bytes", json.len());
                    }
                    Ok(None) => {
                        tracing::debug!("收到诊断数据分片: {}/{}", index.saturating_add(1), total);
                    }
                    Err(e) => {
                        tracing::warn!("诊断数据分片无效: {}", e);
                    }
                }

                let response = http::Response::builder()
                    .status(200)
                    .header("Access-Control-Allow-Origin", "*")
                    .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
                    .header("Access-Control-Allow-Headers", "Content-Type")
                    .body(b"ok".to_vec())
                    .unwrap();
                responder.respond(response);
            } else if uri.contains("collect") {
                let data = decode_diag_collect_payload(&uri, &body);
                if !data.is_empty() {
                    tracing::info!("收到诊断数据: {} bytes", data.len());
                    *captured_store_for_protocol.data.lock().unwrap() = Some(data);
                }
                let response = http::Response::builder()
                    .status(200)
                    .header("Access-Control-Allow-Origin", "*")
                    .header("Access-Control-Allow-Methods", "POST, OPTIONS")
                    .header("Access-Control-Allow-Headers", "Content-Type")
                    .body(b"ok".to_vec())
                    .unwrap();
                responder.respond(response);
            } else if uri.contains("count") {
                let count_str = if body.is_empty() {
                    query_param(&uri, "value")
                        .or_else(|| query_param(&uri, "count"))
                        .unwrap_or_default()
                } else {
                    String::from_utf8(body).unwrap_or_default()
                };
                if let Ok(count) = count_str.trim().parse::<usize>() {
                    tracing::info!("diag://count 更新计数: {}", count);
                    *count_for_protocol.lock().unwrap() = count;
                }
                let response = http::Response::builder()
                    .status(200)
                    .header("Access-Control-Allow-Origin", "*")
                    .header("Access-Control-Allow-Methods", "POST, OPTIONS")
                    .header("Access-Control-Allow-Headers", "Content-Type")
                    .body(b"ok".to_vec())
                    .unwrap();
                responder.respond(response);
            } else {
                let response = http::Response::builder()
                    .status(200)
                    .header("Access-Control-Allow-Origin", "*")
                    .header("Access-Control-Allow-Methods", "POST, OPTIONS")
                    .header("Access-Control-Allow-Headers", "Content-Type")
                    .body(b"ok".to_vec())
                    .unwrap();
                responder.respond(response);
            }
        })
        .invoke_handler(tauri::generate_handler![
            // 第一步：部署文档导入
            generate_service_template,
            generate_db_template,
            export_template,
            import_service_csv,
            import_db_csv,
            set_site_info,
            // 第二步：校验
            validate_services,
            validate_single_service,
            validate_databases,
            list_available_databases,
            list_available_schemas,
            list_available_tables,
            set_selected_database,
            set_selected_schemas,
            confirm_validation,
            // 第三步：WebView 采集
            open_diag_browser,
            collect_diag_data,
            get_capture_count,
            reset_capture_data,
            close_diag_browser,
            open_diag_devtools,
            resolve_request_url,
            start_diagnosis,
            get_config_summary,
            get_desktop_path,
            pick_output_folder,
            // ES 相关
            set_es_config,
            test_es_connection,
            // ELK 相关
            set_elk_config,
            test_elk_connection,
            discover_log_config_from_es,
            set_schedule_config,
            // 日志来源选择
            set_log_source,
            // 调度器
            start_scheduler,
            stop_scheduler,
            get_scheduler_status,
            start_historical_diagnosis,
            // 配置持久化
            save_config_to_disk,
            load_config_from_disk,
            clear_saved_config,
            // 快速诊断
            start_quick_diagnosis,
            open_output_dir,
        ])
        .run(tauri::generate_context!())
        .expect("收集端启动失败");
}
