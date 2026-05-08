use serde::Deserialize;

/// 收集端配置
#[derive(Debug, Clone, Deserialize)]
pub struct CollectorConfig {
    pub site: SiteConfig,
    pub gateway: GatewayConfig,
    pub services: Vec<ServiceConfig>,
    pub ssh: SshConfig,
    pub database: DatabaseConfig,
    pub privacy: PrivacyConfig,
    pub collector: CollectorSettings,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SiteConfig {
    pub name: String,
    pub system: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    pub prefix: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
    pub name: String,
    pub display: String,
    pub hosts: Vec<String>,
    pub log_dir: String,
    pub log_pattern: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SshConfig {
    pub port: u16,
    pub username: String,
    pub auth_type: String, // "key" | "password"
    pub private_key: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    #[serde(rename = "type")]
    pub db_type: String, // "mysql" | "postgresql"
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub database: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrivacyConfig {
    pub mask_query_values: bool,
    pub allowed_query_keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CollectorSettings {
    pub time_window_minutes: u32,
    pub max_log_lines: usize,
    pub output_dir: String,
}

impl CollectorConfig {
    /// 从 TOML 文件加载配置
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: CollectorConfig = toml::from_str(&content)?;
        Ok(config)
    }

    /// 根据服务名查找服务配置
    pub fn find_service(&self, service_name: &str) -> Option<&ServiceConfig> {
        self.services.iter().find(|s| s.name == service_name)
    }
}
