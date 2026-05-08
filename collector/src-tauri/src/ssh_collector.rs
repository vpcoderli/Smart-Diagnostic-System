use anyhow::{anyhow, Result};
use diag_core::config::SshConfig;
use russh::keys::key;
use std::sync::Arc;

/// SSH 远程执行命令并返回输出
pub async fn ssh_exec(host: &str, config: &SshConfig, command: &str) -> Result<String> {
    let key_path = config
        .private_key
        .as_deref()
        .ok_or_else(|| anyhow!("SSH 私钥路径未配置"))?;

    let key_pair = russh_keys::load_secret_key(key_path, None)
        .map_err(|e| anyhow!("加载 SSH 密钥失败 '{}': {}", key_path, e))?;

    let ssh_config = Arc::new(russh::client::Config::default());
    let addr = format!("{}:{}", host, config.port);

    let mut session = russh::client::connect(ssh_config, &addr, Handler)
        .await
        .map_err(|e| anyhow!("SSH 连接 {} 失败: {}", addr, e))?;

    let auth_result = session
        .authenticate_publickey(&config.username, Arc::new(key_pair))
        .await
        .map_err(|e| anyhow!("SSH 认证失败: {}", e))?;

    if !auth_result {
        return Err(anyhow!("SSH 认证失败: 用户 {} 认证被拒绝", config.username));
    }

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
    let cmd = format!(
        "grep -rh '{}' {}{} 2>/dev/null | head -n {}",
        trace_id, log_dir, log_pattern, max_lines
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

// SSH client handler
struct Handler;

#[async_trait::async_trait]
impl russh::client::Handler for Handler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // MVP: 接受所有服务器公钥
        Ok(true)
    }
}
