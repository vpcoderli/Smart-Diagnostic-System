mod commands;
mod rule_engine;
mod report;

use commands::*;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            import_diagnosis_package,
            run_analysis,
            export_report,
        ])
        .run(tauri::generate_context!())
        .expect("分析端启动失败");
}
