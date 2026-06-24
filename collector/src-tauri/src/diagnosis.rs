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

fn has_database_config(config: &diag_core::config::DatabaseConfig) -> bool {
    !config.db_type.trim().is_empty()
        && !config.host.trim().is_empty()
        && config.port > 0
        && !config.database.trim().is_empty()
        && !config.username.trim().is_empty()
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

    pub fn new_historical_with_window(
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

    pub fn historical_window(&self) -> Option<&TimeWindow> {
        self.historical_window.as_ref()
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
        append_collector_warnings(&mut collection_errors, self.log_collector.as_ref());

        // Step 3b: 从日志中提取 SQL traces
        let sql_traces = crate::sql_extractor::extract_sql_traces(&all_logs);
        tracing::info!("SQL trace 提取完成，共 {} 条", sql_traces.len());

        let (slow_sqls, table_stats, explain_plans) = if has_database_config(&self.config.database) {
            // Step 4: 查询慢 SQL + 表统计
            let (slow_sqls, mut table_stats) =
                self.collect_db_data_tracked(&mut collection_errors).await;
            self.collect_sql_trace_table_stats(
                &sql_traces,
                &mut table_stats,
                &mut collection_errors,
            )
            .await;

            // Step 4b: 收集 EXPLAIN 计划
            //   - 来自 db_collector 的慢 SQL（已是规范化文本，可直接 EXPLAIN）
            //   - 来自日志的 SQL trace（需先用 Parameters 行回填占位符再 EXPLAIN）
            let explain_collector = crate::explain_collector::ExplainCollector::new(
                self.config.database.clone(),
                500.0,
            );
            let mut explain_plans = explain_collector.collect_explain_plans(&slow_sqls).await;
            let log_sql_plans = explain_collector
                .collect_explain_for_sql_traces(&sql_traces)
                .await;
            explain_plans.extend(log_sql_plans);
            tracing::info!("EXPLAIN 计划收集完成，共 {} 条", explain_plans.len());
            (slow_sqls, table_stats, explain_plans)
        } else {
            tracing::info!("未配置可用数据库，跳过数据库采集、表统计和 EXPLAIN");
            (Vec::new(), Vec::new(), Vec::new())
        };

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
        let masking_report = url_masking_report(self.captured.as_ref(), &self.config.privacy);

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
                &slow_sqls,
                &explain_plans,
                &table_stats,
                captured,
                self.config.gateway.prefix.as_str(),
                &manifest,
                Some(&report),
                &self.config.privacy,
                Some(&masking_report),
                &output_path,
            )?;
        } else {
            diag_core::package::build_quick_package_with_manifest(
                &all_logs,
                &sql_traces,
                &slow_sqls,
                &explain_plans,
                &table_stats,
                &manifest,
                Some(&report),
                Some(&masking_report),
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

fn append_collector_warnings(errors: &mut Vec<String>, collector: &dyn LogCollector) {
    errors.extend(collector.warnings());
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

fn url_masking_report(
    captured: Option<&CapturedPage>,
    privacy: &diag_core::config::PrivacyConfig,
) -> MaskingReport {
    let Some(captured) = captured else {
        return MaskingReport {
            masked_query_params: Vec::new(),
            removed_headers: Vec::new(),
            masked_sql_params: false,
            total_items_masked: 0,
        };
    };

    if !privacy.mask_query_values {
        return MaskingReport {
            masked_query_params: Vec::new(),
            removed_headers: Vec::new(),
            masked_sql_params: false,
            total_items_masked: 0,
        };
    }

    let allowed_keys: HashSet<&str> = privacy
        .allowed_query_keys
        .iter()
        .map(String::as_str)
        .collect();
    let mut masked_keys: HashSet<String> = HashSet::new();
    let mut total_items_masked = 0;

    if let Ok(page_url) = url::Url::parse(&captured.page_url) {
        collect_masked_query_params(
            &page_url,
            &allowed_keys,
            &mut masked_keys,
            &mut total_items_masked,
        );
    }

    for request in &captured.requests {
        if let Some(request_url) = parse_url_for_masking_report(&request.url, &captured.page_url) {
            collect_masked_query_params(
                &request_url,
                &allowed_keys,
                &mut masked_keys,
                &mut total_items_masked,
            );
        }
    }

    let mut masked_query_params: Vec<String> = masked_keys.into_iter().collect();
    masked_query_params.sort();

    MaskingReport {
        masked_query_params,
        removed_headers: Vec::new(),
        masked_sql_params: false,
        total_items_masked,
    }
}

fn parse_url_for_masking_report(raw_url: &str, page_url: &str) -> Option<url::Url> {
    url::Url::parse(raw_url).ok().or_else(|| {
        let base_url = url::Url::parse(page_url).ok()?;
        base_url.join(raw_url).ok()
    })
}

fn collect_masked_query_params(
    url: &url::Url,
    allowed_keys: &HashSet<&str>,
    masked_keys: &mut HashSet<String>,
    total_items_masked: &mut usize,
) {
    for (key, value) in url.query_pairs() {
        if allowed_keys.contains(key.as_ref()) || value.is_empty() {
            continue;
        }

        masked_keys.insert(key.into_owned());
        *total_items_masked += 1;
    }
}

fn realtime_time_window(captured: &CapturedPage) -> TimeWindow {
    let spans: Vec<(DateTime<FixedOffset>, DateTime<FixedOffset>)> = captured
        .requests
        .iter()
        .filter_map(|request| {
            let end = DateTime::parse_from_rfc3339(&request.timestamp).ok()?;
            let duration_ms = request.duration_ms.min(i64::MAX as u64) as i64;
            let start = end - Duration::milliseconds(duration_ms);
            Some((start, end))
        })
        .collect();

    if spans.is_empty() {
        return TimeWindow {
            start: String::new(),
            end: String::new(),
        };
    }

    let min_start = spans.iter().map(|(start, _)| *start).min().unwrap();
    let max_end = spans.iter().map(|(_, end)| *end).max().unwrap();
    TimeWindow {
        start: (min_start - Duration::minutes(5)).to_rfc3339(),
        end: (max_end + Duration::minutes(5)).to_rfc3339(),
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
