use anyhow::{anyhow, Result};
use diag_core::config::SshConfig;
use russh::keys::key;
use std::sync::Arc;

/// SSH 远程执行命令并返回输出
/// 支持密钥认证和密码认证两种方式
pub async fn ssh_exec(host: &str, config: &SshConfig, command: &str) -> Result<String> {
    let ssh_config = Arc::new(russh::client::Config::default());
    let addr = format!("{}:{}", host, config.port);

    let mut session = russh::client::connect(ssh_config, &addr, Handler)
        .await
        .map_err(|e| anyhow!("SSH 连接 {} 失败: {}", addr, e))?;

    // 根据 auth_type 选择认证方式
    let auth_result = match config.auth_type.as_str() {
        "password" => {
            let password = config
                .password
                .as_deref()
                .ok_or_else(|| anyhow!("SSH 密码认证未配置 password 字段"))?;

            session
                .authenticate_password(&config.username, password)
                .await
                .map_err(|e| anyhow!("SSH 密码认证失败: {}", e))?
        }
        "key" | _ => {
            let key_path = config
                .private_key
                .as_deref()
                .ok_or_else(|| anyhow!("SSH 密钥路径未配置"))?;

            let key_pair = russh_keys::load_secret_key(key_path, None)
                .map_err(|e| anyhow!("加载 SSH 密钥失败 '{}': {}", key_path, e))?;

            session
                .authenticate_publickey(&config.username, Arc::new(key_pair))
                .await
                .map_err(|e| anyhow!("SSH 公钥认证失败: {}", e))?
        }
    };

    if !auth_result {
        return Err(anyhow!(
            "SSH 认证失败: 用户 {} 在 {} 上认证被拒绝 (方式: {})",
            config.username,
            host,
            config.auth_type
        ));
    }

    tracing::info!("SSH 认证成功: {}@{}", config.username, host);

    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|e| anyhow!("打开 SSH channel 失败: {}", e))?;

    channel
        .exec(true, command)
        .await
        .map_err(|e| anyhow!("执行命令失败: {}", e))?;

    let mut output = String::new();
    while let Some(msg) = channel.wait().await {
        match msg {
            russh::ChannelMsg::Data { data } => {
                output.push_str(&String::from_utf8_lossy(&data));
            }
            russh::ChannelMsg::Eof | russh::ChannelMsg::Close => break,
            _ => {}
        }
    }

    Ok(output)
}

/// 通过 SSH grep 远程日志文件
pub async fn grep_remote_logs(
    host: &str,
    ssh_config: &SshConfig,
    log_dir: &str,
    log_pattern: &str,
    trace_id: &str,
    max_lines: usize,
) -> Result<Vec<String>> {
    // 确保 log_dir 以 / 结尾
    let dir = if log_dir.ends_with('/') {
        log_dir.to_string()
    } else {
        format!("{}/", log_dir)
    };

    let cmd = format!(
        "grep -rh '{}' {}{} 2>/dev/null | head -n {}",
        trace_id, dir, log_pattern, max_lines
    );

    tracing::info!("SSH {}@{}: {}", ssh_config.username, host, cmd);

    let output = ssh_exec(host, ssh_config, &cmd).await?;
    let lines: Vec<String> = output
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();

    tracing::info!("从 {} 采集到 {} 行日志", host, lines.len());
    Ok(lines)
}

/// 通过 SSH 获取远程服务器的时间范围内的 ERROR 日志
pub async fn grep_error_logs(
    host: &str,
    ssh_config: &SshConfig,
    log_dir: &str,
    log_pattern: &str,
    time_window_minutes: u32,
    max_lines: usize,
) -> Result<Vec<String>> {
    let dir = if log_dir.ends_with('/') {
        log_dir.to_string()
    } else {
        format!("{}/", log_dir)
    };

    // 使用 find + grep 获取最近修改的日志中的 ERROR
    let cmd = format!(
        "find {} -name '{}' -mmin -{} -exec grep -h 'ERROR\\|Exception\\|FATAL' {{}} + 2>/dev/null | tail -n {}",
        dir, log_pattern, time_window_minutes, max_lines
    );

    tracing::info!("SSH {}@{}: {}", ssh_config.username, host, cmd);

    let output = ssh_exec(host, ssh_config, &cmd).await?;
    let lines: Vec<String> = output
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();

    tracing::info!("从 {} 采集到 {} 行 ERROR 日志", host, lines.len());
    Ok(lines)
}

/// 通过 SSH 列出远程目录下的日志文件
pub async fn list_remote_logs(
    host: &str,
    ssh_config: &SshConfig,
    log_dir: &str,
    log_pattern: &str,
) -> Result<Vec<String>> {
    let dir = if log_dir.ends_with('/') {
        log_dir.to_string()
    } else {
        format!("{}/", log_dir)
    };

    let cmd = format!("ls -lht {}{} 2>/dev/null | head -20", dir, log_pattern);

    let output = ssh_exec(host, ssh_config, &cmd).await?;
    Ok(output.lines().map(String::from).collect())
}

// SSH client handler
struct Handler;

#[async_trait::async_trait]
impl russh::client::Handler for Handler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // MVP: 接受所有服务器公钥（生产环境应校验 known_hosts）
        Ok(true)
    }
}
