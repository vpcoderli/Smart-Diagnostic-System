use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 服务部署信息（模板中的一行）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceDeployment {
    pub project_name: String,
    pub server_ip: String,
    pub ssh_username: String,
    pub ssh_password: String,
    pub ssh_port: u16,
    pub log_path: String,
    pub log_pattern: String,
}

/// 数据库部署信息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseDeployment {
    pub db_type: String,       // mysql / postgresql
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub database: String,
}

/// 完整部署清单
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentManifest {
    pub site_name: String,
    pub system: String,
    pub gateway_prefix: String,
    pub services: Vec<ServiceDeployment>,
    pub databases: Vec<DatabaseDeployment>,
}

/// 校验结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationResult {
    pub target: String,
    pub target_type: String, // "ssh" | "db"
    pub success: bool,
    pub message: String,
    pub details: Option<String>,
}

// ─── 生成模板 ───

/// 生成 CSV 服务部署模板
pub fn generate_service_template() -> String {
    let header = "项目名,服务器IP,SSH用户名,SSH密码,SSH端口,日志路径,日志文件模式";
    let examples = vec![
        "pcm-server,172.29.60.10,deploy,your_password,22,/opt/pcm/pcm-server/logs/,*.log",
        "pcm-followup,172.29.60.11,deploy,your_password,22,/opt/pcm/pcm-followup/logs/,*.log",
        "pcm-communication,172.29.60.12,deploy,your_password,22,/opt/pcm/pcm-communication/logs/,*.log",
        "pcm-management,172.29.60.13,deploy,your_password,22,/opt/pcm/pcm-management/logs/,*.log",
        "pcm-profile,172.29.60.15,deploy,your_password,22,/opt/pcm/pcm-profile/logs/,*.log",
        "pcm-data,172.29.60.16,deploy,your_password,22,/opt/pcm/pcm-data/logs/,*.log",
        "pcm-statistics,172.29.60.17,deploy,your_password,22,/opt/pcm/pcm-statistics/logs/,*.log",
        "pcm-user,172.29.60.18,deploy,your_password,22,/opt/pcm/pcm-user/logs/,*.log",
        "pcm-channel,172.29.60.19,deploy,your_password,22,/opt/pcm/pcm-channel/logs/,*.log",
        "pcm-health-plan,172.29.60.20,deploy,your_password,22,/opt/pcm/pcm-health-plan/logs/,*.log",
        "pcm-open-api,172.29.60.21,deploy,your_password,22,/opt/pcm/pcm-open-api/logs/,*.log",
    ];

    let mut lines = vec![header.to_string()];
    lines.extend(examples.iter().map(|s| s.to_string()));
    lines.join("\n")
}

/// 生成 CSV 数据库部署模板
pub fn generate_db_template() -> String {
    let header = "数据库类型,服务器IP,端口,用户名,密码,数据库名";
    let example = "mysql,172.29.60.100,3306,readonly,your_password,pcm_db";

    format!("{}\n{}", header, example)
}

// ─── 解析模板 ───

/// 从 CSV 内容解析服务部署信息
pub fn parse_service_csv(content: &str) -> Result<Vec<ServiceDeployment>> {
    let mut services = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    if lines.is_empty() {
        return Err(anyhow!("CSV 文件为空"));
    }

    // 跳过表头行
    for (line_no, line) in lines.iter().enumerate().skip(1) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let fields: Vec<&str> = trimmed.split(',').map(|f| f.trim()).collect();
        if fields.len() < 7 {
            return Err(anyhow!(
                "第 {} 行格式错误: 需要 7 个字段，实际 {} 个。内容: '{}'",
                line_no + 1,
                fields.len(),
                trimmed
            ));
        }

        let port: u16 = fields[4]
            .parse()
            .map_err(|_| anyhow!("第 {} 行端口号格式错误: '{}'", line_no + 1, fields[4]))?;

        services.push(ServiceDeployment {
            project_name: fields[0].to_string(),
            server_ip: fields[1].to_string(),
            ssh_username: fields[2].to_string(),
            ssh_password: fields[3].to_string(),
            ssh_port: port,
            log_path: fields[5].to_string(),
            log_pattern: fields[6].to_string(),
        });
    }

    if services.is_empty() {
        return Err(anyhow!("未解析到任何服务配置"));
    }

    Ok(services)
}

/// 从 CSV 内容解析数据库部署信息
pub fn parse_db_csv(content: &str) -> Result<Vec<DatabaseDeployment>> {
    let mut databases = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    if lines.is_empty() {
        return Err(anyhow!("CSV 文件为空"));
    }

    for (line_no, line) in lines.iter().enumerate().skip(1) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let fields: Vec<&str> = trimmed.split(',').map(|f| f.trim()).collect();
        if fields.len() < 6 {
            return Err(anyhow!(
                "第 {} 行格式错误: 需要 6 个字段，实际 {} 个",
                line_no + 1,
                fields.len()
            ));
        }

        let port: u16 = fields[2]
            .parse()
            .map_err(|_| anyhow!("第 {} 行端口号格式错误: '{}'", line_no + 1, fields[2]))?;

        let db_type = match fields[0].to_lowercase().as_str() {
            "mysql" => "mysql",
            "postgresql" | "postgres" | "pg" => "postgresql",
            other => return Err(anyhow!("第 {} 行不支持的数据库类型: '{}'", line_no + 1, other)),
        };

        databases.push(DatabaseDeployment {
            db_type: db_type.to_string(),
            host: fields[1].to_string(),
            port,
            username: fields[3].to_string(),
            password: fields[4].to_string(),
            database: fields[5].to_string(),
        });
    }

    Ok(databases)
}

/// 从 CSV 文件加载服务部署信息
pub fn load_service_csv(path: &Path) -> Result<Vec<ServiceDeployment>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("读取文件失败 '{}': {}", path.display(), e))?;
    parse_service_csv(&content)
}

/// 从 CSV 文件加载数据库部署信息
pub fn load_db_csv(path: &Path) -> Result<Vec<DatabaseDeployment>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("读取文件失败 '{}': {}", path.display(), e))?;
    parse_db_csv(&content)
}

/// 将 DeploymentManifest 转换为 CollectorConfig（兼容已有代码）
pub fn manifest_to_collector_config(manifest: &DeploymentManifest) -> diag_core::config::CollectorConfig {
    use diag_core::config::*;

    // 取第一个数据库配置（MVP 只支持单库）
    let db = manifest.databases.first().cloned().unwrap_or(DatabaseDeployment {
        db_type: "mysql".into(),
        host: "127.0.0.1".into(),
        port: 3306,
        username: "root".into(),
        password: "".into(),
        database: "pcm_db".into(),
    });

    // 取第一个服务的 SSH 配置作为全局 SSH 配置
    let first_svc = manifest.services.first();

    CollectorConfig {
        site: SiteConfig {
            name: manifest.site_name.clone(),
            system: manifest.system.clone(),
        },
        gateway: GatewayConfig {
            prefix: manifest.gateway_prefix.clone(),
        },
        services: manifest
            .services
            .iter()
            .map(|s| ServiceConfig {
                name: s.project_name.clone(),
                display: diag_core::url_resolver::service_display_name(&s.project_name).to_string(),
                hosts: vec![s.server_ip.clone()],
                log_dir: s.log_path.clone(),
                log_pattern: s.log_pattern.clone(),
            })
            .collect(),
        ssh: SshConfig {
            port: first_svc.map(|s| s.ssh_port).unwrap_or(22),
            username: first_svc.map(|s| s.ssh_username.clone()).unwrap_or_default(),
            auth_type: "password".to_string(),
            private_key: None,
            password: first_svc.map(|s| s.ssh_password.clone()),
        },
        database: DatabaseConfig {
            db_type: db.db_type,
            host: db.host,
            port: db.port,
            username: db.username,
            password: db.password,
            database: db.database,
        },
        privacy: PrivacyConfig {
            mask_query_values: true,
            allowed_query_keys: vec![
                "pageNum".into(), "pageSize".into(), "portal".into(),
                "type".into(), "status".into(),
            ],
        },
        collector: CollectorSettings {
            time_window_minutes: 30,
            max_log_lines: 500,
            output_dir: "./diagnosis-output".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_service_csv() {
        let csv = r#"项目名,服务器IP,SSH用户名,SSH密码,SSH端口,日志路径,日志文件模式
pcm-server,172.29.60.10,deploy,pass123,22,/opt/pcm/pcm-server/logs/,*.log
pcm-user,172.29.60.18,deploy,pass123,22,/opt/pcm/pcm-user/logs/,*.log"#;

        let services = parse_service_csv(csv).unwrap();
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].project_name, "pcm-server");
        assert_eq!(services[0].server_ip, "172.29.60.10");
        assert_eq!(services[0].ssh_port, 22);
        assert_eq!(services[1].project_name, "pcm-user");
    }

    #[test]
    fn test_parse_db_csv() {
        let csv = r#"数据库类型,服务器IP,端口,用户名,密码,数据库名
mysql,172.29.60.100,3306,readonly,pass123,pcm_db"#;

        let dbs = parse_db_csv(csv).unwrap();
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].db_type, "mysql");
        assert_eq!(dbs[0].host, "172.29.60.100");
        assert_eq!(dbs[0].port, 3306);
    }

    #[test]
    fn test_parse_empty_csv() {
        assert!(parse_service_csv("").is_err());
        assert!(parse_service_csv("header\n").is_err()); // only header, no data
    }

    #[test]
    fn test_parse_bad_format() {
        let csv = "header\nonly,two,fields";
        assert!(parse_service_csv(csv).is_err());
    }

    #[test]
    fn test_generate_templates() {
        let svc_tpl = generate_service_template();
        assert!(svc_tpl.contains("项目名"));
        assert!(svc_tpl.contains("pcm-server"));

        let db_tpl = generate_db_template();
        assert!(db_tpl.contains("数据库类型"));
        assert!(db_tpl.contains("mysql"));
    }

    #[test]
    fn test_manifest_to_config() {
        let manifest = DeploymentManifest {
            site_name: "test-hospital".into(),
            system: "pcm".into(),
            gateway_prefix: "/gateway".into(),
            services: vec![ServiceDeployment {
                project_name: "pcm-server".into(),
                server_ip: "10.0.1.1".into(),
                ssh_username: "admin".into(),
                ssh_password: "pass".into(),
                ssh_port: 22,
                log_path: "/opt/logs/".into(),
                log_pattern: "*.log".into(),
            }],
            databases: vec![DatabaseDeployment {
                db_type: "mysql".into(),
                host: "10.0.1.100".into(),
                port: 3306,
                username: "readonly".into(),
                password: "dbpass".into(),
                database: "pcm_db".into(),
            }],
        };

        let config = manifest_to_collector_config(&manifest);
        assert_eq!(config.site.name, "test-hospital");
        assert_eq!(config.services.len(), 1);
        assert_eq!(config.database.host, "10.0.1.100");
        assert_eq!(config.ssh.auth_type, "password");
    }
}
