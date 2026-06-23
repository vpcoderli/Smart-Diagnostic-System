use anyhow::Result;
use diag_core::config::SshConfig;

use crate::deployment::{DatabaseDeployment, ServiceDeployment, ValidationResult};
use crate::ssh_collector;

/// 本地模式校验：直接检查本地文件系统
fn validate_service_local(svc: &ServiceDeployment) -> ValidationResult {
    let log_dir = std::path::Path::new(&svc.log_path);
    if !log_dir.exists() {
        return ValidationResult {
            target: format!("{}@{}", svc.project_name, svc.server_ip),
            target_type: "ssh".to_string(),
            success: false,
            message: format!("本地日志目录不存在: {}", svc.log_path),
            details: None,
        };
    }

    let pattern = svc.log_pattern.replace("*", "");
    let files: Vec<String> = std::fs::read_dir(log_dir)
        .unwrap_or_else(|_| std::fs::read_dir("/dev/null").unwrap())
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| pattern.is_empty() || name.contains(&pattern))
        .take(5)
        .collect();

    if files.is_empty() {
        ValidationResult {
            target: format!("{}@{}", svc.project_name, svc.server_ip),
            target_type: "local".to_string(),
            success: false,
            message: format!("日志目录存在但未找到匹配 '{}' 的文件", svc.log_pattern),
            details: None,
        }
    } else {
        ValidationResult {
            target: format!("{}@{}", svc.project_name, svc.server_ip),
            target_type: "local".to_string(),
            success: true,
            message: format!("本地路径 ✓ | 找到 {} 个日志文件", files.len()),
            details: Some(files.join("\n")),
        }
    }
}

/// 校验单个服务的 SSH 连通性 + 日志路径
pub async fn validate_service(svc: &ServiceDeployment) -> ValidationResult {
    let is_local = svc.server_ip == "127.0.0.1" || svc.server_ip == "localhost";

    if is_local {
        return validate_service_local(svc);
    }

    let ssh_config = SshConfig {
        port: svc.ssh_port,
        username: svc.ssh_username.clone(),
        auth_type: "password".to_string(),
        private_key: None,
        password: Some(svc.ssh_password.clone()),
    };

    // Step 1: SSH 连接测试
    let test_cmd = "echo 'DIAG_SSH_OK'";
    match ssh_collector::ssh_exec(&svc.server_ip, &ssh_config, test_cmd).await {
        Err(e) => {
            return ValidationResult {
                target: format!("{}@{}", svc.project_name, svc.server_ip),
                target_type: "ssh".to_string(),
                success: false,
                message: format!("SSH 连接失败: {}", e),
                details: None,
            };
        }
        Ok(output) => {
            if !output.contains("DIAG_SSH_OK") {
                return ValidationResult {
                    target: format!("{}@{}", svc.project_name, svc.server_ip),
                    target_type: "ssh".to_string(),
                    success: false,
                    message: "SSH 连接异常: 回显验证失败".to_string(),
                    details: Some(output),
                };
            }
        }
    }

    // Step 2: 校验日志路径是否存在
    let log_dir = if svc.log_path.ends_with('/') {
        svc.log_path.clone()
    } else {
        format!("{}/", svc.log_path)
    };

    let check_cmd = format!(
        "test -d '{}' && ls {}{} 2>/dev/null | head -5 || echo 'DIAG_DIR_NOT_FOUND'",
        log_dir.trim_end_matches('/'),
        log_dir,
        svc.log_pattern
    );

    match ssh_collector::ssh_exec(&svc.server_ip, &ssh_config, &check_cmd).await {
        Err(e) => ValidationResult {
            target: format!("{}@{}", svc.project_name, svc.server_ip),
            target_type: "ssh".to_string(),
            success: false,
            message: format!("日志路径检查失败: {}", e),
            details: None,
        },
        Ok(output) => {
            if output.contains("DIAG_DIR_NOT_FOUND") {
                ValidationResult {
                    target: format!("{}@{}", svc.project_name, svc.server_ip),
                    target_type: "ssh".to_string(),
                    success: false,
                    message: format!("日志目录不存在: {}", svc.log_path),
                    details: None,
                }
            } else if output.trim().is_empty() {
                ValidationResult {
                    target: format!("{}@{}", svc.project_name, svc.server_ip),
                    target_type: "ssh".to_string(),
                    success: false,
                    message: format!("日志目录存在但未找到匹配 '{}' 的日志文件", svc.log_pattern),
                    details: None,
                }
            } else {
                let file_list: Vec<&str> = output.lines().take(5).collect();
                ValidationResult {
                    target: format!("{}@{}", svc.project_name, svc.server_ip),
                    target_type: "ssh".to_string(),
                    success: true,
                    message: format!(
                        "SSH 连通 ✓ | 日志路径 ✓ | 找到 {} 个日志文件",
                        file_list.len()
                    ),
                    details: Some(file_list.join("\n")),
                }
            }
        }
    }
}

/// 校验数据库连通性
pub async fn validate_database(db: &DatabaseDeployment) -> ValidationResult {
    let target = format!("{}://{}:{}/{}", db.db_type, db.host, db.port, db.database);

    match db.db_type.as_str() {
        "mysql" => validate_mysql(db, &target).await,
        "postgresql" | "postgres" => validate_postgresql(db, &target).await,
        _ => ValidationResult {
            target,
            target_type: "db".to_string(),
            success: false,
            message: format!("不支持的数据库类型: {}", db.db_type),
            details: None,
        },
    }
}

async fn validate_mysql(db: &DatabaseDeployment, target: &str) -> ValidationResult {
    let url = format!(
        "mysql://{}:{}@{}:{}/{}",
        db.username, db.password, db.host, db.port, db.database
    );

    match sqlx::MySqlPool::connect(&url).await {
        Err(e) => ValidationResult {
            target: target.to_string(),
            target_type: "db".to_string(),
            success: false,
            message: format!("MySQL 连接失败: {}", e),
            details: None,
        },
        Ok(pool) => {
            // 验证: 查询版本 + 检查 performance_schema
            let version: Result<String, _> = sqlx::query_scalar("SELECT VERSION()")
                .fetch_one(&pool)
                .await;

            let perf_schema: Result<i64, _> = sqlx::query_scalar(
                "SELECT COUNT(*) FROM information_schema.tables WHERE TABLE_SCHEMA = 'performance_schema' AND TABLE_NAME = 'events_statements_summary_by_digest'"
            )
            .fetch_one(&pool)
            .await;

            pool.close().await;

            let ver = version.unwrap_or_else(|_| "未知".to_string());
            let has_perf = perf_schema.unwrap_or(0) > 0;

            ValidationResult {
                target: target.to_string(),
                target_type: "db".to_string(),
                success: true,
                message: format!(
                    "MySQL 连通 ✓ | 版本: {} | performance_schema: {}",
                    ver,
                    if has_perf {
                        "✓ 可用"
                    } else {
                        "✗ 不可用（慢SQL采集受限）"
                    }
                ),
                details: None,
            }
        }
    }
}

async fn validate_postgresql(db: &DatabaseDeployment, target: &str) -> ValidationResult {
    let url = format!(
        "postgres://{}:{}@{}:{}/{}",
        db.username, db.password, db.host, db.port, db.database
    );

    match sqlx::PgPool::connect(&url).await {
        Err(e) => ValidationResult {
            target: target.to_string(),
            target_type: "db".to_string(),
            success: false,
            message: format!("PostgreSQL 连接失败: {}", e),
            details: None,
        },
        Ok(pool) => {
            let version: Result<String, _> = sqlx::query_scalar("SELECT version()")
                .fetch_one(&pool)
                .await;

            // 检查 pg_stat_statements 扩展
            let has_pgss: Result<i64, _> = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_extension WHERE extname = 'pg_stat_statements'",
            )
            .fetch_one(&pool)
            .await;

            pool.close().await;

            let ver = version.unwrap_or_else(|_| "未知".to_string());
            let has_ext = has_pgss.unwrap_or(0) > 0;

            ValidationResult {
                target: target.to_string(),
                target_type: "db".to_string(),
                success: true,
                message: format!(
                    "PostgreSQL 连通 ✓ | {} | pg_stat_statements: {}",
                    if ver.len() > 40 { &ver[..40] } else { &ver },
                    if has_ext {
                        "✓ 已安装"
                    } else {
                        "✗ 未安装（慢SQL采集受限）"
                    }
                ),
                details: None,
            }
        }
    }
}

/// 列出 PG/MySQL 上的所有可访问数据库
pub async fn list_databases(db: &DatabaseDeployment) -> Result<Vec<String>, String> {
    match db.db_type.as_str() {
        "mysql" => list_mysql_databases(db).await,
        "postgresql" | "postgres" => list_postgresql_databases(db).await,
        _ => Err(format!("不支持的数据库类型: {}", db.db_type)),
    }
}

/// 列出 PostgreSQL 中指定数据库的所有 schema（过滤系统 schema）
pub async fn list_schemas_in_database(db: &DatabaseDeployment) -> Result<Vec<String>, String> {
    if !matches!(db.db_type.as_str(), "postgresql" | "postgres") {
        return Err("list_schemas 仅支持 PostgreSQL".to_string());
    }

    let url = format!(
        "postgres://{}:{}@{}:{}/{}",
        db.username, db.password, db.host, db.port, db.database
    );

    let pool = sqlx::PgPool::connect(&url)
        .await
        .map_err(|e| format!("连接数据库失败: {}", e))?;

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT schema_name FROM information_schema.schemata \
         WHERE schema_name NOT IN ('pg_catalog', 'information_schema', 'pg_toast', 'pg_temp_1', 'pg_toast_temp_1') \
         ORDER BY schema_name"
    )
    .fetch_all(&pool).await
    .map_err(|e| format!("查询 schema 失败: {}", e))?;

    pool.close().await;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

async fn list_mysql_databases(db: &DatabaseDeployment) -> Result<Vec<String>, String> {
    // 用 information_schema 作为默认连接库（用户填写的库可能还没确定）
    let initial_db = if db.database.is_empty() {
        "information_schema"
    } else {
        &db.database
    };
    let url = format!(
        "mysql://{}:{}@{}:{}/{}",
        db.username, db.password, db.host, db.port, initial_db
    );

    let pool = sqlx::MySqlPool::connect(&url)
        .await
        .map_err(|e| format!("MySQL 连接失败: {}", e))?;

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
         WHERE SCHEMA_NAME NOT IN ('information_schema','performance_schema','mysql','sys') \
         ORDER BY SCHEMA_NAME",
    )
    .fetch_all(&pool)
    .await
    .map_err(|e| format!("查询数据库列表失败: {}", e))?;

    pool.close().await;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

async fn list_postgresql_databases(db: &DatabaseDeployment) -> Result<Vec<String>, String> {
    // 用 postgres 作为默认连接库
    let initial_db = if db.database.is_empty() {
        "postgres"
    } else {
        &db.database
    };
    let url = format!(
        "postgres://{}:{}@{}:{}/{}",
        db.username, db.password, db.host, db.port, initial_db
    );

    let pool = sqlx::PgPool::connect(&url)
        .await
        .map_err(|e| format!("PostgreSQL 连接失败: {}", e))?;

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT datname FROM pg_database \
         WHERE datistemplate = false AND datname NOT IN ('postgres') \
         ORDER BY datname",
    )
    .fetch_all(&pool)
    .await
    .map_err(|e| format!("查询数据库列表失败: {}", e))?;

    pool.close().await;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

/// 批量校验所有服务
pub async fn validate_all_services(services: &[ServiceDeployment]) -> Vec<ValidationResult> {
    let mut results = Vec::new();
    for svc in services {
        tracing::info!("校验服务: {} @ {}", svc.project_name, svc.server_ip);
        let result = validate_service(svc).await;
        tracing::info!(
            "  → {} : {}",
            if result.success { "✓" } else { "✗" },
            result.message
        );
        results.push(result);
    }
    results
}

/// 批量校验所有数据库
pub async fn validate_all_databases(databases: &[DatabaseDeployment]) -> Vec<ValidationResult> {
    let mut results = Vec::new();
    for db in databases {
        tracing::info!("校验数据库: {}:{}/{}", db.host, db.port, db.database);
        let result = validate_database(db).await;
        tracing::info!(
            "  → {} : {}",
            if result.success { "✓" } else { "✗" },
            result.message
        );
        results.push(result);
    }
    results
}

/// 列出数据库服务器上选定数据库/模式下的表列表
pub async fn list_tables(
    db: &DatabaseDeployment,
    schemas: Vec<String>,
) -> Result<Vec<String>, String> {
    match db.db_type.as_str() {
        "mysql" => list_mysql_tables(db).await,
        "postgresql" | "postgres" => list_postgresql_tables(db, schemas).await,
        _ => Err(format!("不支持的数据库类型: {}", db.db_type)),
    }
}

async fn list_mysql_tables(db: &DatabaseDeployment) -> Result<Vec<String>, String> {
    if db.database.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!(
        "mysql://{}:{}@{}:{}/{}",
        db.username, db.password, db.host, db.port, db.database
    );
    let pool = sqlx::MySqlPool::connect(&url)
        .await
        .map_err(|e| format!("MySQL 连接失败: {}", e))?;
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT TABLE_NAME FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' \
         ORDER BY TABLE_NAME",
    )
    .bind(&db.database)
    .fetch_all(&pool)
    .await
    .map_err(|e| format!("查询表列表失败: {}", e))?;
    pool.close().await;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

async fn list_postgresql_tables(
    db: &DatabaseDeployment,
    schemas: Vec<String>,
) -> Result<Vec<String>, String> {
    if db.database.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!(
        "postgres://{}:{}@{}:{}/{}",
        db.username, db.password, db.host, db.port, db.database
    );
    let pool = sqlx::PgPool::connect(&url)
        .await
        .map_err(|e| format!("PostgreSQL 连接失败: {}", e))?;

    let schemas = if schemas.is_empty() {
        vec!["public".to_string()]
    } else {
        schemas
    };

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = ANY($1) AND table_type = 'BASE TABLE' \
         ORDER BY table_name",
    )
    .bind(&schemas)
    .fetch_all(&pool)
    .await
    .map_err(|e| format!("查询表列表失败: {}", e))?;
    pool.close().await;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}
