use anyhow::Result;
use chrono::Utc;
use diag_core::collector_trait::LogCollector;
use diag_core::config::CollectorConfig;
use diag_core::models::*;
use diag_core::url_resolver;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// 诊断流程执行器
pub struct DiagnosisRunner {
    config: CollectorConfig,
    captured: Option<CapturedPage>,
    log_collector: Box<dyn LogCollector>,
    trace_ids: Vec<String>,
}

impl DiagnosisRunner {
    /// 实时模式：从浏览器捕获的页面数据出发
    pub fn new(
        config: CollectorConfig,
        captured: CapturedPage,
        log_collector: Box<dyn LogCollector>,
    ) -> Self {
        Self {
            config,
            captured: Some(captured),
            log_collector,
            trace_ids: Vec::new(),
        }
    }

    /// 历史模式：直接提供 traceId 列表，不依赖浏览器捕获
    pub fn new_historical(
        config: CollectorConfig,
        log_collector: Box<dyn LogCollector>,
        trace_ids: Vec<String>,
    ) -> Self {
        Self {
            config,
            captured: None,
            log_collector,
            trace_ids,
        }
    }

    /// 执行完整诊断流程，返回 diagnosis.zip 路径
    pub async fn run(&self) -> Result<String> {
        let page_url = self
            .captured
            .as_ref()
            .map(|c| c.page_url.as_str())
            .unwrap_or("historical");
        tracing::info!("开始诊断流程，页面: {}", page_url);

        // 贯穿整个 run() 的错误和跳过收集器
        let mut collection_errors: Vec<String> = Vec::new();
        let mut skipped_services: Vec<String> = Vec::new();

        // Step 1: 收集 traceId 列表
        let trace_ids: Vec<String> = if let Some(captured) = &self.captured {
            captured
                .requests
                .iter()
                .filter_map(|r| r.trace_id.clone())
                .collect()
        } else {
            self.trace_ids.clone()
        };

        // Step 2: 从请求时间戳推算时间窗（前后各加 5 分钟）
        let window = if let Some(captured) = &self.captured {
            let timestamps: Vec<&str> = captured
                .requests
                .iter()
                .map(|r| r.timestamp.as_str())
                .collect();
            let min_ts = timestamps.iter().min().copied().unwrap_or("");
            let max_ts = timestamps.iter().max().copied().unwrap_or("");
            TimeWindow {
                start: min_ts.to_string(),
                end: max_ts.to_string(),
            }
        } else {
            TimeWindow {
                start: String::new(),
                end: String::new(),
            }
        };

        // Step 3: 通过 LogCollector trait 采集日志
        let all_logs = match self
            .log_collector
            .query_by_trace_ids(&trace_ids, None, &window)
            .await
        {
            Ok(entries) => {
                tracing::info!("日志采集完成，共 {} 条", entries.len());
                entries
            }
            Err(e) => {
                let msg = format!("日志采集失败: {}", e);
                tracing::warn!("{}（跳过）", msg);
                collection_errors.push(msg);
                Vec::new()
            }
        };

        // Step 3b: 从日志中提取 SQL traces
        let sql_traces = crate::sql_extractor::extract_sql_traces(&all_logs);
        tracing::info!("SQL trace 提取完成，共 {} 条", sql_traces.len());

        // Step 4: 查询慢 SQL + 表统计
        let (slow_sqls, mut table_stats) =
            self.collect_db_data_tracked(&mut collection_errors).await;
        self.collect_sql_trace_table_stats(&sql_traces, &mut table_stats, &mut collection_errors)
            .await;

        // Step 4b: 收集 EXPLAIN 计划
        //   - 来自 db_collector 的慢 SQL（已是规范化文本，可直接 EXPLAIN）
        //   - 来自日志的 SQL trace（需先用 Parameters 行回填占位符再 EXPLAIN）
        let explain_collector =
            crate::explain_collector::ExplainCollector::new(self.config.database.clone(), 500.0);
        let mut explain_plans = explain_collector.collect_explain_plans(&slow_sqls).await;
        let log_sql_plans = explain_collector
            .collect_explain_for_sql_traces(&sql_traces)
            .await;
        explain_plans.extend(log_sql_plans);
        tracing::info!("EXPLAIN 计划收集完成，共 {} 条", explain_plans.len());

        // 统计跳过的服务（仅实时模式有请求，历史模式无需此步）
        let grouped = self.group_requests_by_service();
        for (service_name, _) in &grouped {
            if service_name == "unknown" {
                skipped_services.push("unknown（URL 无法解析服务名）".to_string());
            } else if self.config.find_service(service_name).is_none() {
                skipped_services.push(format!("{} （未在配置中找到）", service_name));
            }
        }

        // Step 6: 准备打包
        let now = Utc::now();

        // Step 8: 打包（统一使用 TXT 格式，与快速诊断一致）
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

        if let Some(captured) = &self.captured {
            diag_core::package::build_realtime_package(
                &all_logs,
                &sql_traces,
                &explain_plans,
                &table_stats,
                captured,
                self.config.gateway.prefix.as_str(),
                &output_path,
            )?;
        } else {
            diag_core::package::build_quick_package(
                &all_logs,
                &sql_traces,
                &explain_plans,
                &table_stats,
                &output_path,
            )?;
        }

        let result = output_path.to_string_lossy().to_string();
        tracing::info!("诊断包已生成: {}", result);
        Ok(result)
    }

    /// 按服务名分组请求，无法解析的 URL 归入 "unknown" 组
    fn group_requests_by_service(&self) -> HashMap<String, Vec<&CapturedRequest>> {
        let mut grouped: HashMap<String, Vec<&CapturedRequest>> = HashMap::new();

        let requests = match &self.captured {
            Some(c) => &c.requests[..],
            None => return grouped,
        };

        for req in requests {
            let service = url_resolver::resolve_url(&req.url, &self.config.gateway.prefix)
                .map(|r| r.service)
                .unwrap_or_else(|_| "unknown".to_string());
            grouped.entry(service).or_default().push(req);
        }

        grouped
    }

    /// 采集数据库慢 SQL 和表统计，并将错误追加到 errors 集合中
    async fn collect_db_data_tracked(
        &self,
        errors: &mut Vec<String>,
    ) -> (Vec<SlowSqlItem>, Vec<TableStats>) {
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
                let msg = format!("数据库采集失败: {}", e);
                tracing::warn!("{}（跳过）", msg);
                errors.push(msg);
                (Vec::new(), Vec::new())
            }
        }
    }

    async fn collect_sql_trace_table_stats(
        &self,
        sql_traces: &[SqlTrace],
        table_stats: &mut Vec<TableStats>,
        errors: &mut Vec<String>,
    ) {
        let table_names = sql_trace_table_names(sql_traces);
        if table_names.is_empty() {
            return;
        }

        let collector = crate::db_collector::DbCollector::new(self.config.database.clone());
        match collector.collect_table_stats_for_tables(&table_names).await {
            Ok(extra_stats) => {
                let extra_count = extra_stats.len();
                merge_table_stats(table_stats, extra_stats);
                tracing::info!("日志 SQL 涉及表统计补采完成: {} 张", extra_count);
            }
            Err(e) => {
                let msg = format!("日志 SQL 涉及表统计补采失败: {}", e);
                tracing::warn!("{}", msg);
                errors.push(msg);
            }
        }
    }
}

fn sql_trace_table_names(sql_traces: &[SqlTrace]) -> Vec<String> {
    let mut names: Vec<String> = sql_traces
        .iter()
        .flat_map(|trace| trace.tables.iter().cloned())
        .filter(|name| !name.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    names.sort();
    names
}

fn merge_table_stats(base: &mut Vec<TableStats>, extra: Vec<TableStats>) {
    let mut seen: HashSet<(String, String)> = base
        .iter()
        .map(|s| (s.schema.clone(), s.table_name.clone()))
        .collect();

    for stat in extra {
        let key = (stat.schema.clone(), stat.table_name.clone());
        if seen.insert(key) {
            base.push(stat);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sql_trace_table_names_are_deduplicated() {
        let traces = vec![
            SqlTrace {
                trace_id: "t1".into(),
                service: "svc".into(),
                sql: "select * from tb_name_list".into(),
                sql_fingerprint: "select * from tb_name_list".into(),
                duration_ms: None,
                tables: vec!["tb_name_list".into(), "tb_dept".into()],
                timestamp: None,
                parameters: None,
            },
            SqlTrace {
                trace_id: "t2".into(),
                service: "svc".into(),
                sql: "select * from tb_name_list".into(),
                sql_fingerprint: "select * from tb_name_list".into(),
                duration_ms: None,
                tables: vec!["tb_name_list".into()],
                timestamp: None,
                parameters: None,
            },
        ];

        assert_eq!(
            sql_trace_table_names(&traces),
            vec!["tb_dept".to_string(), "tb_name_list".to_string()]
        );
    }

    #[test]
    fn test_merge_table_stats_keeps_distinct_schema_table_pairs() {
        let mut base = vec![TableStats {
            schema: "public".into(),
            table_name: "tb_name_list".into(),
            row_count: 1,
            data_size_bytes: None,
            index_size_bytes: None,
            indexes: vec![],
        }];
        let extra = vec![
            TableStats {
                schema: "public".into(),
                table_name: "tb_name_list".into(),
                row_count: 2,
                data_size_bytes: None,
                index_size_bytes: None,
                indexes: vec![],
            },
            TableStats {
                schema: "outbound_platform".into(),
                table_name: "tb_name_list".into(),
                row_count: 3,
                data_size_bytes: None,
                index_size_bytes: None,
                indexes: vec![],
            },
        ];

        merge_table_stats(&mut base, extra);

        assert_eq!(base.len(), 2);
        assert!(base
            .iter()
            .any(|s| s.schema == "outbound_platform" && s.row_count == 3));
    }
}
