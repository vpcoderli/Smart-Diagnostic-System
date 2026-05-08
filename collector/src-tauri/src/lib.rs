mod commands;
mod db_collector;
mod deployment;
mod diagnosis;
mod ssh_collector;
mod validator;

use commands::*;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    tracing::info!("Smart Diagnostic Collector 启动中...");

    tauri::Builder::default()
        .manage(AppState {
            manifest: std::sync::Mutex::new(None),
            config: std::sync::Mutex::new(None),
            validated: std::sync::Mutex::new(false),
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
            confirm_validation,
            // 第三步：采集
            resolve_request_url,
            start_diagnosis,
            get_config_summary,
        ])
        .run(tauri::generate_context!())
        .expect("收集端启动失败");
}
