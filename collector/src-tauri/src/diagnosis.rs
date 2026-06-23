use anyhow::Result;
use chrono::{DateTime, Duration, FixedOffset, Utc};
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
    historical_window: Option<TimeWindow>,
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
            historical_window: None,
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
            historical_window: None,
        }
    }

    #[cfg(test)]
    fn new_historical_with_window(
        config: CollectorConfig,
        log_collector: Box<dyn LogCollector>,
        trace_ids: Vec<String>,
        window: TimeWindow,
    ) -> Self {
        Self {
            config,
            captured: None,
            log_collector,
            trace_ids,
            historical_window: Some(window),
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
            realtime_time_window(captured)
        } else {
            self.historical_window.clone().unwrap_or(TimeWindow {
                start: String::new(),
                end: String::new(),
            })
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
        let manifest_window =
            if self.captured.is_none() && window.start.is_empty() && window.end.is_empty() {
                log_time_window(&all_logs)
            } else {
                window.clone()
            };
        let manifest = diagnosis_manifest(
            self,
            &now,
            self.captured.as_ref(),
            &manifest_window,
            &trace_ids,
            &all_logs,
            &sql_traces,
        );
        let report = collection_report(
            &now,
            self.log_collector.source_type(),
            all_logs.len(),
            sql_traces.len(),
            explain_plans.len(),
            &skipped_services,
            &collection_errors,
        );

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
            diag_core::package::build_realtime_package_with_manifest(
                &all_logs,
                &sql_traces,
                &explain_plans,
                &table_stats,
                captured,
                self.config.gateway.prefix.as_str(),
                &manifest,
                Some(&report),
                &output_path,
            )?;
        } else {
            diag_core::package::build_quick_package_with_manifest(
                &all_logs,
                &sql_traces,
                &explain_plans,
                &table_stats,
                &manifest,
                Some(&report),
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

fn diagnosis_manifest(
    runner: &DiagnosisRunner,
    now: &DateTime<Utc>,
    captured: Option<&CapturedPage>,
    window: &TimeWindow,
    trace_ids: &[String],
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
) -> DiagnosisManifest {
    let mut services: Vec<String> = logs
        .iter()
        .map(|log| log.service.clone())
        .chain(sql_traces.iter().map(|trace| trace.service.clone()))
        .chain(
            captured
                .map(|page| {
                    page.requests
                        .iter()
                        .filter_map(|request| {
                            url_resolver::resolve_url(&request.url, &runner.config.gateway.prefix)
                                .ok()
                                .map(|resolved| resolved.service)
                        })
                        .filter(|service| service != "unknown")
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        )
        .filter(|service| !service.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    services.sort();

    let mut trace_ids: Vec<String> = trace_ids
        .iter()
        .filter(|trace_id| !trace_id.is_empty())
        .cloned()
        .chain(logs.iter().filter_map(|log| log.trace_id.clone()))
        .chain(
            sql_traces
                .iter()
                .map(|trace| trace.trace_id.clone())
                .filter(|trace_id| !trace_id.is_empty()),
        )
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    trace_ids.sort();

    let time_range = if window.start.is_empty() && window.end.is_empty() {
        None
    } else {
        Some(window.clone())
    };

    DiagnosisManifest {
        diagnosis_id: format!("diag-{}", now.format("%Y%m%d-%H%M%S")),
        site: runner.config.site.name.clone(),
        system: runner.config.site.system.clone(),
        created_at: now.to_rfc3339(),
        page_url: captured
            .map(|page| page.page_url.clone())
            .unwrap_or_else(|| "historical".to_string()),
        request_count: captured.map(|page| page.requests.len()).unwrap_or(0),
        services,
        trace_ids,
        database_type: runner.config.database.db_type.clone(),
        privacy_level: "MASKED".into(),
        collector_version: env!("CARGO_PKG_VERSION").to_string(),
        collection_mode: Some(
            if captured.is_some() {
                "realtime"
            } else {
                "historical"
            }
            .to_string(),
        ),
        log_source: Some(runner.log_collector.source_type().to_string()),
        gateway_prefix: Some(runner.config.gateway.prefix.clone()),
        keywords: None,
        time_range,
    }
}

fn collection_report(
    now: &DateTime<Utc>,
    log_source: &str,
    log_count: usize,
    sql_trace_count: usize,
    explain_plan_count: usize,
    skipped_services: &[String],
    errors: &[String],
) -> CollectionReport {
    CollectionReport {
        collected_at: now.to_rfc3339(),
        log_source: log_source.to_string(),
        log_count,
        sql_trace_count,
        explain_plan_count,
        skipped_services: skipped_services.to_vec(),
        errors: errors.to_vec(),
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

fn realtime_time_window(captured: &CapturedPage) -> TimeWindow {
    let parsed: Vec<DateTime<FixedOffset>> = captured
        .requests
        .iter()
        .filter_map(|request| DateTime::parse_from_rfc3339(&request.timestamp).ok())
        .collect();

    if parsed.is_empty() {
        return TimeWindow {
            start: String::new(),
            end: String::new(),
        };
    }

    let min_ts = parsed.iter().min().cloned().unwrap();
    let max_ts = parsed.iter().max().cloned().unwrap();
    TimeWindow {
        start: (min_ts - Duration::minutes(5)).to_rfc3339(),
        end: (max_ts + Duration::minutes(5)).to_rfc3339(),
    }
}

fn log_time_window(logs: &[LogEntry]) -> TimeWindow {
    let parsed: Vec<DateTime<FixedOffset>> = logs
        .iter()
        .filter_map(|entry| entry.time.as_deref())
        .filter_map(|time| DateTime::parse_from_rfc3339(time).ok())
        .collect();

    if parsed.is_empty() {
        return TimeWindow {
            start: String::new(),
            end: String::new(),
        };
    }

    let min_ts = parsed.iter().min().cloned().unwrap();
    let max_ts = parsed.iter().max().cloned().unwrap();
    TimeWindow {
        start: (min_ts - Duration::minutes(5)).to_rfc3339(),
        end: (max_ts + Duration::minutes(5)).to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use diag_core::collector_trait::LogCollector;
    use diag_core::config::{
        CollectorSettings, DatabaseConfig, GatewayConfig, PrivacyConfig, ServiceConfig, SiteConfig,
        SshConfig,
    };

    struct StubLogCollector;

    #[async_trait]
    impl LogCollector for StubLogCollector {
        async fn query_by_trace_ids(
            &self,
            _trace_ids: &[String],
            _service: Option<&str>,
            _window: &TimeWindow,
        ) -> Result<Vec<LogEntry>> {
            Ok(Vec::new())
        }

        async fn query_by_keywords(
            &self,
            _keywords: &[String],
            _service: Option<&str>,
            _window: &TimeWindow,
        ) -> Result<Vec<LogEntry>> {
            Ok(Vec::new())
        }

        fn source_type(&self) -> &'static str {
            "elk"
        }
    }

    fn test_config() -> CollectorConfig {
        CollectorConfig {
            site: SiteConfig {
                name: "协和医院".into(),
                system: "pcm-core".into(),
            },
            gateway: GatewayConfig {
                prefix: "/custom-gateway".into(),
            },
            services: vec![
                ServiceConfig {
                    name: "pcm-management".into(),
                    display: "管理服务".into(),
                    hosts: vec!["10.0.0.1".into()],
                    log_dir: "/logs/management".into(),
                    log_pattern: "*.log".into(),
                },
                ServiceConfig {
                    name: "pcm-user".into(),
                    display: "用户服务".into(),
                    hosts: vec!["10.0.0.2".into()],
                    log_dir: "/logs/user".into(),
                    log_pattern: "*.log".into(),
                },
            ],
            ssh: SshConfig {
                port: 22,
                username: "ops".into(),
                auth_type: "password".into(),
                private_key: None,
                password: Some("pass".into()),
            },
            database: DatabaseConfig {
                db_type: "postgresql".into(),
                host: "10.0.0.100".into(),
                port: 5432,
                username: "readonly".into(),
                password: "secret".into(),
                database: "pcm".into(),
                schemas: vec!["public".into()],
            },
            privacy: PrivacyConfig {
                mask_query_values: true,
                allowed_query_keys: vec!["pageNum".into(), "pageSize".into()],
            },
            collector: CollectorSettings {
                time_window_minutes: 30,
                max_log_lines: 500,
                output_dir: "diagnosis-output".into(),
            },
            elk: None,
            nacos: None,
            schedule: None,
        }
    }

    fn captured_page() -> CapturedPage {
        CapturedPage {
            page_url: "http://10.0.0.1/custom-gateway/pcm-management/v1/patient/list?pageNum=1"
                .into(),
            requests: vec![
                CapturedRequest {
                    method: "GET".into(),
                    url: "http://10.0.0.1/custom-gateway/pcm-management/v1/patient/list?pageNum=1"
                        .into(),
                    status: 200,
                    duration_ms: 1200,
                    trace_id: Some("trace-2".into()),
                    timestamp: "2026-06-23T10:00:00+08:00".into(),
                    request_type: "fetch".into(),
                    response_size: Some(1024),
                },
                CapturedRequest {
                    method: "GET".into(),
                    url: "http://10.0.0.1/custom-gateway/pcm-user/v1/profile?pageSize=20".into(),
                    status: 200,
                    duration_ms: 300,
                    trace_id: Some("trace-1".into()),
                    timestamp: "2026-06-23T10:00:02+08:00".into(),
                    request_type: "xhr".into(),
                    response_size: Some(256),
                },
            ],
        }
    }

    fn sample_logs() -> Vec<LogEntry> {
        vec![
            LogEntry {
                time: Some("2026-06-23T10:00:01+08:00".into()),
                level: "ERROR".into(),
                service: "pcm-management".into(),
                trace_id: Some("trace-2".into()),
                thread: None,
                class: None,
                method: None,
                message: "管理服务异常".into(),
                exception: None,
                stack_trace: None,
                raw: "raw-1".into(),
            },
            LogEntry {
                time: Some("2026-06-23T10:00:03+08:00".into()),
                level: "WARN".into(),
                service: "pcm-user".into(),
                trace_id: Some("trace-1".into()),
                thread: None,
                class: None,
                method: None,
                message: "用户服务告警".into(),
                exception: None,
                stack_trace: None,
                raw: "raw-2".into(),
            },
        ]
    }

    fn sample_sql_traces() -> Vec<SqlTrace> {
        vec![SqlTrace {
            trace_id: "trace-2".into(),
            service: "pcm-management".into(),
            sql: "select * from patient".into(),
            sql_fingerprint: "select * from patient".into(),
            duration_ms: Some(1234.0),
            tables: vec!["patient".into()],
            timestamp: Some("2026-06-23T10:00:01+08:00".into()),
            parameters: None,
        }]
    }

    fn sample_window() -> TimeWindow {
        TimeWindow {
            start: "2026-06-23T09:55:00+08:00".into(),
            end: "2026-06-23T10:05:00+08:00".into(),
        }
    }

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

    #[test]
    fn test_realtime_time_window_pads_captured_timestamps() {
        let captured = CapturedPage {
            page_url: "http://host/page".into(),
            requests: vec![CapturedRequest {
                method: "GET".into(),
                url: "http://host/gateway/pcm-management/v1/list".into(),
                status: 200,
                duration_ms: 150,
                trace_id: Some("trace-1".into()),
                timestamp: "2026-06-03T12:00:00Z".into(),
                request_type: "fetch".into(),
                response_size: None,
            }],
        };

        let window = realtime_time_window(&captured);
        assert_eq!(window.start, "2026-06-03T11:55:00+00:00");
        assert_eq!(window.end, "2026-06-03T12:05:00+00:00");
    }

    #[test]
    fn test_log_time_window_pads_log_timestamps() {
        let logs = vec![LogEntry {
            time: Some("2026-06-23T10:00:00+08:00".into()),
            level: "INFO".into(),
            service: "pcm-management".into(),
            trace_id: Some("trace-1".into()),
            thread: None,
            class: None,
            method: None,
            message: "ok".into(),
            exception: None,
            stack_trace: None,
            raw: "raw".into(),
        }];

        let window = log_time_window(&logs);
        assert_eq!(window.start, "2026-06-23T09:55:00+08:00");
        assert_eq!(window.end, "2026-06-23T10:05:00+08:00");
    }

    #[test]
    fn diagnosis_manifest_realtime_preserves_config_and_capture_metadata() {
        let config = test_config();
        let captured = captured_page();
        let runner =
            DiagnosisRunner::new(config.clone(), captured.clone(), Box::new(StubLogCollector));
        let logs = sample_logs();
        let sql_traces = sample_sql_traces();
        let window = sample_window();
        let now = Utc::now();

        let manifest = diagnosis_manifest(
            &runner,
            &now,
            Some(&captured),
            &window,
            &["trace-2".into(), "trace-1".into()],
            &logs,
            &sql_traces,
        );

        assert!(manifest.diagnosis_id.starts_with("diag-"));
        assert_eq!(manifest.site, "协和医院");
        assert_eq!(manifest.system, "pcm-core");
        assert_eq!(manifest.database_type, "postgresql");
        assert_eq!(manifest.log_source.as_deref(), Some("elk"));
        assert_eq!(manifest.gateway_prefix.as_deref(), Some("/custom-gateway"));
        assert_eq!(manifest.collection_mode.as_deref(), Some("realtime"));
        assert_eq!(manifest.request_count, 2);
        assert_eq!(manifest.page_url, captured.page_url);
        assert_eq!(
            manifest.services,
            vec!["pcm-management".to_string(), "pcm-user".to_string()]
        );
        assert_eq!(
            manifest.trace_ids,
            vec!["trace-1".to_string(), "trace-2".to_string()]
        );
        let time_range = manifest.time_range.expect("应写入时间窗");
        assert_eq!(time_range.start, window.start);
        assert_eq!(time_range.end, window.end);
    }

    #[test]
    fn diagnosis_manifest_historical_uses_historical_defaults() {
        let config = test_config();
        let runner = DiagnosisRunner::new_historical_with_window(
            config.clone(),
            Box::new(StubLogCollector),
            vec!["trace-2".into(), "trace-1".into()],
            sample_window(),
        );
        let logs = sample_logs();
        let sql_traces = sample_sql_traces();
        let window = sample_window();
        let now = Utc::now();

        let manifest = diagnosis_manifest(
            &runner,
            &now,
            None,
            &window,
            &["trace-2".into(), "trace-1".into()],
            &logs,
            &sql_traces,
        );

        assert_eq!(manifest.collection_mode.as_deref(), Some("historical"));
        assert_eq!(manifest.page_url, "historical");
        assert_eq!(manifest.request_count, 0);
        assert_eq!(manifest.log_source.as_deref(), Some("elk"));
        assert_eq!(manifest.database_type, "postgresql");
        assert_eq!(
            manifest.trace_ids,
            vec!["trace-1".to_string(), "trace-2".to_string()]
        );
        assert!(manifest.keywords.is_none());
        assert!(manifest.time_range.is_some());
    }

    #[test]
    fn collection_report_preserves_errors_and_skipped_services() {
        let now = Utc::now();
        let report = collection_report(
            &now,
            "ssh",
            3,
            1,
            2,
            &[
                "unknown（URL 无法解析服务名）".to_string(),
                "pcm-missing （未在配置中找到）".to_string(),
            ],
            &[
                "日志采集失败: timeout".to_string(),
                "数据库采集失败: denied".to_string(),
            ],
        );

        assert_eq!(report.log_source, "ssh");
        assert_eq!(report.log_count, 3);
        assert_eq!(report.sql_trace_count, 1);
        assert_eq!(report.explain_plan_count, 2);
        assert_eq!(
            report.skipped_services,
            vec![
                "unknown（URL 无法解析服务名）".to_string(),
                "pcm-missing （未在配置中找到）".to_string()
            ]
        );
        assert_eq!(
            report.errors,
            vec![
                "日志采集失败: timeout".to_string(),
                "数据库采集失败: denied".to_string()
            ]
        );
        assert_eq!(report.collected_at, now.to_rfc3339());
    }
}
