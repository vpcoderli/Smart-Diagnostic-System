use std::path::Path;
use std::time::{Duration, SystemTime};

/// 清理超过 retention_days 天的诊断包。
/// 扫描 output_dir 下所有 .zip 文件，按修改时间判断是否超龄。
pub fn cleanup_old_packages(output_dir: &str, retention_days: u32) {
    let dir = Path::new(output_dir);
    if !dir.exists() {
        return;
    }

    let max_age = Duration::from_secs(retention_days as u64 * 24 * 3600);
    let now = SystemTime::now();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("清理目录读取失败 {}: {}", output_dir, e);
            return;
        }
    };

    let mut deleted = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        // 只清理 .zip 文件
        if path.extension().and_then(|e| e.to_str()) != Some("zip") {
            continue;
        }

        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|modified| {
                now.duration_since(modified)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            })
            .unwrap_or(Duration::ZERO);

        if age > max_age {
            match std::fs::remove_file(&path) {
                Ok(_) => {
                    deleted += 1;
                    tracing::info!("清理旧诊断包: {}", path.display());
                }
                Err(e) => {
                    tracing::warn!("删除失败 {}: {}", path.display(), e);
                }
            }
        }
    }

    if deleted > 0 {
        tracing::info!("本次清理 {} 个旧诊断包", deleted);
    }
}
