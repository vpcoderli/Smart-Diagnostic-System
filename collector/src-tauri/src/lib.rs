mod commands;
mod ssh_collector;
mod diagnosis;

use commands::*;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            load_config,
            resolve_request_url,
            start_diagnosis,
            get_diagnosis_status,
        ])
        .run(tauri::generate_context!())
        .expect("收集端启动失败");
}
