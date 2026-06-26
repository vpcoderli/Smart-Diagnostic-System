use serde::{Deserialize, Serialize};

// ─── ELK 字段映射 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldMapping {
    pub timestamp: String,
    pub level: String,
    pub service: String,
    pub trace_id: String,
    pub message: String,
    pub exception: String,
    pub stack_trace: String,
    pub thread: String,
}

impl Default for FieldMapping {
    fn default() -> Self {
        Self {
            timestamp: "@timestamp".into(),
            level: "level".into(),
            service: "serviceName".into(),
            trace_id: "traceId".into(),
            message: "message".into(),
            exception: "exception".into(),
            stack_trace: "stackTrace".into(),
            thread: "thread".into(),
        }
    }
}

// ─── ELK 配置 ───

fn default_timeout() -> u64 { 30 }
fn default_max_hits() -> usize { 1000 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElkConfig {
    pub address: String,
    pub index_pattern: String,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_hits")]
    pub max_hits_per_trace: usize,
    #[serde(default)]
    pub field_mapping: FieldMapping,
}

impl Default for ElkConfig {
    fn default() -> Self {
        Self {
            address: String::new(),
            index_pattern: "logstash-*".into(),
            username: None,
            password: None,
            timeout_secs: 30,
            max_hits_per_trace: 1000,
            field_mapping: FieldMapping::default(),
        }
    }
}

// ─── ES 直接连接配置 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EsConfig {
    pub address: String,
    pub index_pattern: String,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_hits")]
    pub max_hits_per_trace: usize,
    #[serde(default)]
    pub field_mapping: FieldMapping,
}

impl Default for EsConfig {
    fn default() -> Self {
        Self {
            address: String::new(),
            index_pattern: "logstash-*".into(),
            username: None,
            password: None,
            timeout_secs: 30,
            max_hits_per_trace: 1000,
            field_mapping: FieldMapping::default(),
        }
    }
}

// ─── Nacos 配置 ───

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NacosConfig {
    pub address: String,
    pub namespace: String,
    pub group: String,
    pub service_prefix: String,
    pub log_path_pattern: String,
}

// ─── 定时巡检配置 ───

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleConfig {
    pub enabled: bool,
    pub interval_minutes: u32,
    pub lookback_minutes: u32,
    pub overlap_minutes: u32,
    pub levels: Vec<String>,
    pub extra_keywords: Vec<String>,
    pub service_filter: Option<Vec<String>>,
    pub max_trace_ids_per_run: usize,
    pub dedup_window_minutes: u32,
    pub output_retention_days: u32,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 5,
            lookback_minutes: 6,
            overlap_minutes: 1,
            levels: vec!["ERROR".into(), "WARN".into()],
            extra_keywords: vec![],
            service_filter: None,
            max_trace_ids_per_run: 50,
            dedup_window_minutes: 60,
            output_retention_days: 7,
        }
    }
}

// ─── 收集端配置 ───

#[derive(Debug, Clone, Deserialize)]
pub struct CollectorConfig {
    pub site: SiteConfig,
    pub gateway: GatewayConfig,
    pub services: Vec<ServiceConfig>,
    pub ssh: SshConfig,
    pub database: DatabaseConfig,
    pub privacy: PrivacyConfig,
    pub collector: CollectorSettings,
    #[serde(default)]
    pub elk: Option<ElkConfig>,
    #[serde(default)]
    pub es: Option<EsConfig>,
    #[serde(default)]
    pub nacos: Option<NacosConfig>,
    #[serde(default)]
    pub schedule: Option<ScheduleConfig>,
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
    /// PostgreSQL 模式（schema）列表。MySQL 留空。
    /// 按顺序 SET search_path，PG 会按顺序查找表（首个匹配生效）。
    #[serde(default)]
    pub schemas: Vec<String>,
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
