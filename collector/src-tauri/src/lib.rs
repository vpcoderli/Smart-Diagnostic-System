mod commands;
mod ssh_collector;
mod db_collector;
mod diagnosis;

use commands::*;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    tracing::info!("Smart Diagnostic Collector 启动中...");

    tauri::Builder::default()
        .manage(AppState {
            config: std::sync::Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            load_config,
            resolve_request_url,
            resolve_batch_urls,
            test_ssh_connection,
            list_remote_log_files,
            test_db_connection,
            start_diagnosis,
            get_diagnosis_status,
        ])
        .run(tauri::generate_context!())
        .expect("收集端启动失败");
}
