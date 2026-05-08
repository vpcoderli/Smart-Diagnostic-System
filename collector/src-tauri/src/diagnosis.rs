use anyhow::Result;
use chrono::Utc;
use diag_core::config::CollectorConfig;
use diag_core::log_parser;
use diag_core::masking;
use diag_core::models::*;
use diag_core::url_resolver;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::ssh_collector;

/// 诊断流程执行器
pub struct DiagnosisRunner {
    config: CollectorConfig,
    captured: CapturedPage,
}

impl DiagnosisRunner {
    pub fn new(config: CollectorConfig, captured: CapturedPage) -> Self {
        Self { config, captured }
    }

    /// 执行完整诊断流程，返回 diagnosis.zip 路径
    pub async fn run(&self) -> Result<String> {
        tracing::info!("开始诊断流程，页面: {}", self.captured.page_url);

        // Step 1: 解析所有请求，按服务分组
        let grouped = self.group_requests_by_service();
        tracing::info!("识别到 {} 个服务", grouped.len());

        // Step 2: 对每个服务，SSH 采集日志
        let mut all_logs: Vec<LogEntry> = Vec::new();
        for (service_name, requests) in &grouped {
            let logs = self.collect_service_logs(service_name, requests).await;
            match logs {
                Ok(entries) => {
                    tracing::info!("服务 {} 采集到 {} 条日志", service_name, entries.len());
                    all_logs.extend(entries);
                }
                Err(e) => {
                    tracing::warn!("服务 {} 日志采集失败: {}", service_name, e);
                }
            }
        }

        // Step 3: 查询慢 SQL + 表统计
        let (slow_sqls, table_stats) = self.collect_db_data().await;

        // Step 4: 构建诊断包
        let now = Utc::now();
        let diagnosis_id = format!("diag-{}", now.format("%Y%m%d-%H%M%S"));
        let services: Vec<String> = grouped.keys().cloned().collect();
        let trace_ids: Vec<String> = self
            .captured
            .requests
            .iter()
            .filter_map(|r| r.trace_id.clone())
            .collect();

        let manifest = DiagnosisManifest {
            diagnosis_id: diagnosis_id.clone(),
            site: self.config.site.name.clone(),
            system: self.config.site.system.clone(),
            created_at: now.to_rfc3339(),
            page_url: self.captured.page_url.clone(),
            request_count: self.captured.requests.len(),
            services,
            trace_ids,
            database_type: self.config.database.db_type.clone(),
            privacy_level: "MASKED".to_string(),
            collector_version: "0.1.0".to_string(),
        };

        // Step 5: 脱敏请求 URL
        let masked_requests: Vec<CapturedRequest> = self
            .captured
            .requests
            .iter()
            .map(|r| {
                let masked_url = masking::mask_url(&r.url, &self.config.privacy);
                CapturedRequest {
                    url: masked_url,
                    ..r.clone()
                }
            })
            .collect();

        let package = DiagnosisPackage {
            manifest,
            captured_page: CapturedPage {
                page_url: self.captured.page_url.clone(),
                requests: masked_requests,
            },
            logs: all_logs,
            slow_sqls,
            table_stats,
        };

        let masking_report = MaskingReport {
            masked_query_params: vec!["已对非白名单参数进行脱敏".to_string()],
            removed_headers: vec!["authorization".into(), "cookie".into()],
            masked_sql_params: true,
            total_items_masked: self.captured.requests.len(),
        };

        // Step 6: 打包
        let filename = format!(
            "diagnosis-{}-{}.zip",
            self.config.site.name,
            now.format("%Y%m%d-%H%M%S")
        );
        let output_path = PathBuf::from(&self.config.collector.output_dir).join(&filename);

        // 确保输出目录存在
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        diag_core::package::build_package(&package, &masking_report, &output_path)?;

        let result = output_path.to_string_lossy().to_string();
        tracing::info!("诊断包已生成: {}", result);
        Ok(result)
    }

    /// 按服务名分组请求
    fn group_requests_by_service(&self) -> HashMap<String, Vec<&CapturedRequest>> {
        let mut grouped: HashMap<String, Vec<&CapturedRequest>> = HashMap::new();

        for req in &self.captured.requests {
            if let Ok(resolved) =
                url_resolver::resolve_url(&req.url, &self.config.gateway.prefix)
            {
                grouped
                    .entry(resolved.service)
                    .or_default()
                    .push(req);
            }
        }

        grouped
    }

    /// 采集单个服务的日志
    async fn collect_service_logs(
        &self,
        service_name: &str,
        requests: &[&CapturedRequest],
    ) -> Result<Vec<LogEntry>> {
        let service_config = self
            .config
            .find_service(service_name)
            .ok_or_else(|| anyhow::anyhow!("未找到服务配置: {}", service_name))?;

        // 收集所有 traceId
        let trace_ids: Vec<&str> = requests
            .iter()
            .filter_map(|r| r.trace_id.as_deref())
            .collect();

        if trace_ids.is_empty() {
            tracing::warn!("服务 {} 的请求中没有 traceId，跳过日志采集", service_name);
            return Ok(vec![]);
        }

        let mut all_entries = Vec::new();

        for host in &service_config.hosts {
            for trace_id in &trace_ids {
                match ssh_collector::grep_remote_logs(
                    host,
                    &self.config.ssh,
                    &service_config.log_dir,
                    &service_config.log_pattern,
                    trace_id,
                    self.config.collector.max_log_lines,
                )
                .await
                {
                    Ok(lines) => {
                        for line in &lines {
                            let entry = log_parser::parse_log_line(line, service_name);
                            all_entries.push(entry);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "从 {} 采集服务 {} traceId={} 日志失败: {}",
                            host, service_name, trace_id, e
                        );
                    }
                }
            }
        }

        Ok(all_entries)
    }

    /// 采集数据库慢 SQL 和表统计
    async fn collect_db_data(&self) -> (Vec<SlowSqlItem>, Vec<TableStats>) {
        let collector = crate::db_collector::DbCollector::new(self.config.database.clone());
        match collector.collect().await {
            Ok((sqls, stats)) => {
                tracing::info!(
                    "数据库采集完成: {} 条慢 SQL, {} 张表统计",
                    sqls.len(),
                    stats.len()
                );
                (sqls, stats)
            }
            Err(e) => {
                tracing::warn!("数据库采集失败（跳过）: {}", e);
                (Vec::new(), Vec::new())
            }
        }
    }
}
