use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::deployment::{DatabaseDeployment, DeploymentManifest, ServiceDeployment};

const CONFIG_FILE: &str = "deployment-config.json";

/// 持久化的部署配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredConfig {
    pub version: u32,
    pub last_modified: String,
    pub manifest: DeploymentManifest,
}

impl StoredConfig {
    pub fn new(manifest: DeploymentManifest) -> Self {
        Self {
            version: 1,
            last_modified: chrono::Utc::now().to_rfc3339(),
            manifest,
        }
    }
}

/// 获取配置文件路径（应用数据目录下）
pub fn config_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(CONFIG_FILE)
}

/// 保存配置到本地
pub fn save_config(app_data_dir: &Path, manifest: &DeploymentManifest) -> Result<PathBuf> {
    let dir = app_data_dir;
    std::fs::create_dir_all(dir)?;

    let path = config_path(dir);
    let stored = StoredConfig::new(manifest.clone());
    let json = serde_json::to_string_pretty(&stored)?;
    std::fs::write(&path, json)?;

    tracing::info!("配置已保存到: {}", path.display());
    Ok(path)
}

/// 从本地加载配置
pub fn load_config(app_data_dir: &Path) -> Result<DeploymentManifest> {
    let path = config_path(app_data_dir);
    if !path.exists() {
        return Err(anyhow!("本地配置不存在"));
    }

    let json = std::fs::read_to_string(&path)?;
    let stored: StoredConfig = serde_json::from_str(&json)?;
    tracing::info!(
        "配置已加载: {} 个服务, {} 个数据库",
        stored.manifest.services.len(),
        stored.manifest.databases.len()
    );
    Ok(stored.manifest)
}

/// 检查本地是否有已保存的配置
pub fn has_saved_config(app_data_dir: &Path) -> bool {
    config_path(app_data_dir).exists()
}

/// 删除本地配置
pub fn delete_config(app_data_dir: &Path) -> Result<()> {
    let path = config_path(app_data_dir);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

// ─── 服务 CRUD ───

/// 添加服务
pub fn add_service(manifest: &mut DeploymentManifest, svc: ServiceDeployment) -> Result<()> {
    if manifest
        .services
        .iter()
        .any(|s| s.project_name == svc.project_name)
    {
        return Err(anyhow!("服务 {} 已存在", svc.project_name));
    }
    manifest.services.push(svc);
    Ok(())
}

/// 更新服务
pub fn update_service(
    manifest: &mut DeploymentManifest,
    index: usize,
    svc: ServiceDeployment,
) -> Result<()> {
    if index >= manifest.services.len() {
        return Err(anyhow!("索引 {} 越界", index));
    }
    manifest.services[index] = svc;
    Ok(())
}

/// 删除服务
pub fn remove_service(
    manifest: &mut DeploymentManifest,
    index: usize,
) -> Result<ServiceDeployment> {
    if index >= manifest.services.len() {
        return Err(anyhow!("索引 {} 越界", index));
    }
    Ok(manifest.services.remove(index))
}

/// 按名称查找服务
pub fn find_service(manifest: &DeploymentManifest, name: &str) -> Option<usize> {
    manifest
        .services
        .iter()
        .position(|s| s.project_name == name)
}

// ─── 数据库 CRUD ───

pub fn add_database(manifest: &mut DeploymentManifest, db: DatabaseDeployment) -> Result<()> {
    manifest.databases.push(db);
    Ok(())
}

pub fn update_database(
    manifest: &mut DeploymentManifest,
    index: usize,
    db: DatabaseDeployment,
) -> Result<()> {
    if index >= manifest.databases.len() {
        return Err(anyhow!("索引 {} 越界", index));
    }
    manifest.databases[index] = db;
    Ok(())
}

pub fn remove_database(
    manifest: &mut DeploymentManifest,
    index: usize,
) -> Result<DatabaseDeployment> {
    if index >= manifest.databases.len() {
        return Err(anyhow!("索引 {} 越界", index));
    }
    Ok(manifest.databases.remove(index))
}
