use crate::config::PrivacyConfig;
use crate::masking;
use crate::models::{
    CapturedPage, CapturedRequest, CollectionReport, DiagnosisManifest, DiagnosisPackage,
    ExplainPlan, LogEntry, MaskingReport, SlowSqlItem, SqlTrace, TableStats,
};
use crate::url_resolver;
use anyhow::Result;
use chrono::{DateTime, Duration, FixedOffset, Utc};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;

/// 将诊断数据打包为 diagnosis.zip
pub fn build_package(
    package: &DiagnosisPackage,
    masking_report: &MaskingReport,
    output_path: &Path,
) -> Result<()> {
    let file = std::fs::File::create(output_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    write_structured_contents(
        &mut zip,
        options,
        &package.manifest,
        &package.captured_page,
        &package.logs,
        &package.sql_traces,
        &package.slow_sqls,
        &package.explain_plans,
        &package.table_stats,
        package.collection_report.as_ref(),
        Some(masking_report),
    )?;

    zip.finish()?;
    Ok(())
}

fn write_structured_contents<W: Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    options: zip::write::SimpleFileOptions,
    manifest: &DiagnosisManifest,
    captured_page: &CapturedPage,
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    slow_sqls: &[SlowSqlItem],
    explain_plans: &[ExplainPlan],
    table_stats: &[TableStats],
    collection_report: Option<&CollectionReport>,
    masking_report: Option<&MaskingReport>,
) -> Result<()> {
    zip.start_file("manifest.json", options)?;
    zip.write_all(serde_json::to_string_pretty(manifest)?.as_bytes())?;

    zip.start_file("browser/page.json", options)?;
    let page_info = serde_json::json!({ "pageUrl": captured_page.page_url });
    zip.write_all(serde_json::to_string_pretty(&page_info)?.as_bytes())?;

    zip.start_file("browser/requests.json", options)?;
    zip.write_all(serde_json::to_string_pretty(&captured_page.requests)?.as_bytes())?;

    let mut logs_by_service: HashMap<&str, Vec<&LogEntry>> = HashMap::new();
    for log in logs {
        logs_by_service
            .entry(log.service.as_str())
            .or_default()
            .push(log);
    }
    for (service, entries) in &logs_by_service {
        let file_path = format!("services/{}/app-log.jsonl", service);
        zip.start_file(&file_path, options)?;
        for entry in entries {
            zip.write_all(serde_json::to_string(entry)?.as_bytes())?;
            zip.write_all(b"\n")?;
        }
    }

    if !sql_traces.is_empty() {
        zip.start_file("database/sql-traces.json", options)?;
        zip.write_all(serde_json::to_string_pretty(sql_traces)?.as_bytes())?;
    }

    if !slow_sqls.is_empty() {
        zip.start_file("database/slow-sql.json", options)?;
        zip.write_all(serde_json::to_string_pretty(slow_sqls)?.as_bytes())?;
    }

    if !explain_plans.is_empty() {
        zip.start_file("database/explain-plans.json", options)?;
        zip.write_all(serde_json::to_string_pretty(explain_plans)?.as_bytes())?;
    }

    if !table_stats.is_empty() {
        zip.start_file("database/table-stats.json", options)?;
        zip.write_all(serde_json::to_string_pretty(table_stats)?.as_bytes())?;
    }

    write_collection_report(zip, options, collection_report)?;

    if let Some(masking_report) = masking_report {
        zip.start_file("privacy/masking-report.json", options)?;
        zip.write_all(serde_json::to_string_pretty(masking_report)?.as_bytes())?;
    }

    Ok(())
}

fn write_collection_report<W: Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    options: zip::write::SimpleFileOptions,
    collection_report: Option<&CollectionReport>,
) -> Result<()> {
    if let Some(report) = collection_report {
        zip.start_file("collection_report/report.json", options)?;
        zip.write_all(serde_json::to_string_pretty(report)?.as_bytes())?;
    }

    Ok(())
}

fn synthetic_manifest(
    captured_page: &CapturedPage,
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    collection_mode: &str,
    diagnosis_id: &str,
    gateway_prefix: Option<&str>,
) -> DiagnosisManifest {
    let mut services: Vec<String> = logs
        .iter()
        .map(|log| log.service.clone())
        .chain(sql_traces.iter().map(|trace| trace.service.clone()))
        .filter(|service| !service.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    services.sort();

    let mut trace_ids: Vec<String> = captured_page
        .requests
        .iter()
        .filter_map(|request| request.trace_id.clone())
        .chain(logs.iter().filter_map(|log| log.trace_id.clone()))
        .chain(
            sql_traces
                .iter()
                .map(|trace| trace.trace_id.clone())
                .filter(|trace_id| !trace_id.is_empty()),
        )
        .filter(|trace_id| !trace_id.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    trace_ids.sort();

    DiagnosisManifest {
        diagnosis_id: diagnosis_id.to_string(),
        site: "unknown".into(),
        system: "unknown".into(),
        created_at: Utc::now().to_rfc3339(),
        page_url: captured_page.page_url.clone(),
        request_count: captured_page.requests.len(),
        services,
        trace_ids,
        database_type: "unknown".into(),
        privacy_level: "MASKED".into(),
        collector_version: "quick".into(),
        collection_mode: Some(collection_mode.to_string()),
        log_source: None,
        gateway_prefix: gateway_prefix.map(str::to_string),
        keywords: None,
        time_range: None,
    }
}

fn default_report_privacy_config() -> PrivacyConfig {
    PrivacyConfig {
        mask_query_values: true,
        allowed_query_keys: vec![
            "pageNum".into(),
            "pageSize".into(),
            "portal".into(),
            "type".into(),
            "status".into(),
        ],
    }
}

fn masked_captured_page(captured_page: &CapturedPage, privacy: &PrivacyConfig) -> CapturedPage {
    let mut masked_page = captured_page.clone();
    masked_page.page_url = masking::mask_url(&captured_page.page_url, privacy);
    for request in &mut masked_page.requests {
        request.url = mask_captured_request_url(&request.url, &captured_page.page_url, privacy);
    }
    masked_page
}

fn mask_captured_request_url(raw_url: &str, page_url: &str, privacy: &PrivacyConfig) -> String {
    if url::Url::parse(raw_url).is_ok() {
        return masking::mask_url(raw_url, privacy);
    }

    let Ok(base_url) = url::Url::parse(page_url) else {
        return raw_url.to_string();
    };
    let Ok(joined_url) = base_url.join(raw_url) else {
        return raw_url.to_string();
    };

    masking::mask_url(joined_url.as_ref(), privacy)
}

/// 从 diagnosis.zip 读取诊断包
pub fn read_package(zip_path: &Path) -> Result<DiagnosisPackage> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    // 读取 manifest
    let manifest: DiagnosisManifest = {
        let mut f = archive.by_name("manifest.json")?;
        let mut buf = String::new();
        f.read_to_string(&mut buf)?;
        serde_json::from_str(&buf)?
    };

    // 读取 requests
    let captured_page = {
        let page_url = manifest.page_url.clone();
        let mut f = archive.by_name("browser/requests.json")?;
        let mut buf = String::new();
        f.read_to_string(&mut buf)?;
        let requests = serde_json::from_str(&buf)?;
        crate::models::CapturedPage { page_url, requests }
    };

    // 读取 slow-sql
    let slow_sqls = match archive.by_name("database/slow-sql.json") {
        Ok(mut f) => {
            let mut buf = String::new();
            f.read_to_string(&mut buf)?;
            serde_json::from_str(&buf)?
        }
        Err(_) => vec![],
    };

    // 读取 sql-traces
    let sql_traces = match archive.by_name("database/sql-traces.json") {
        Ok(mut f) => {
            let mut buf = String::new();
            f.read_to_string(&mut buf)?;
            serde_json::from_str(&buf)
                .map_err(|e| anyhow::anyhow!("解析 database/sql-traces.json 失败: {}", e))?
        }
        Err(_) => vec![],
    };

    // 读取 explain-plans
    let explain_plans = match archive.by_name("database/explain-plans.json") {
        Ok(mut f) => {
            let mut buf = String::new();
            f.read_to_string(&mut buf)?;
            serde_json::from_str(&buf)
                .map_err(|e| anyhow::anyhow!("解析 database/explain-plans.json 失败: {}", e))?
        }
        Err(_) => vec![],
    };

    // 读取 table-stats
    let table_stats = match archive.by_name("database/table-stats.json") {
        Ok(mut f) => {
            let mut buf = String::new();
            f.read_to_string(&mut buf)?;
            serde_json::from_str(&buf)?
        }
        Err(_) => vec![],
    };

    // 读取日志（遍历所有 services/*/app-log.jsonl）
    let mut logs = vec![];
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| n.starts_with("services/") && n.ends_with("app-log.jsonl"))
        .collect();

    for name in names {
        if let Ok(mut f) = archive.by_name(&name) {
            let mut buf = String::new();
            f.read_to_string(&mut buf)?;
            for line in buf.lines() {
                if let Ok(entry) = serde_json::from_str(line) {
                    logs.push(entry);
                }
            }
        }
    }

    // 读取 collection_report
    let collection_report =
        match archive.by_name("collection_report/report.json") {
            Ok(mut f) => {
                let mut buf = String::new();
                f.read_to_string(&mut buf)?;
                Some(serde_json::from_str(&buf).map_err(|e| {
                    anyhow::anyhow!("解析 collection_report/report.json 失败: {}", e)
                })?)
            }
            Err(_) => None,
        };

    Ok(DiagnosisPackage {
        manifest,
        captured_page,
        logs,
        slow_sqls,
        table_stats,
        collection_report,
        sql_traces,
        explain_plans,
    })
}

// ═══════════════════════════════════════
// 快速诊断 TXT 格式打包
// ═══════════════════════════════════════

/// 快速诊断模式：输出 TXT 格式的 ZIP 包。
///
/// 适用于没有慢 SQL 数据的调用方；如果已有慢 SQL、采集报告或更完整的元数据，
/// 请优先使用 `build_quick_package_with_manifest()` 或其它 richer API。
pub fn build_quick_package(
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    explain_plans: &[ExplainPlan],
    table_stats: &[TableStats],
    output_path: &Path,
) -> Result<()> {
    let captured_page = CapturedPage {
        page_url: "quick".into(),
        requests: Vec::new(),
    };
    let manifest = synthetic_manifest(
        &captured_page,
        logs,
        sql_traces,
        "quick",
        "quick-package",
        None,
    );
    build_quick_package_with_manifest(
        logs,
        sql_traces,
        &[],
        explain_plans,
        table_stats,
        &manifest,
        None,
        None,
        output_path,
    )
}

/// 实时诊断模式：在快速诊断包内容基础上，额外输出按浏览器请求 traceId 分组的日志。
pub fn build_realtime_package(
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    explain_plans: &[ExplainPlan],
    table_stats: &[TableStats],
    captured_page: &CapturedPage,
    gateway_prefix: &str,
    output_path: &Path,
) -> Result<()> {
    let privacy = default_report_privacy_config();
    let masked_page = masked_captured_page(captured_page, &privacy);
    let manifest = synthetic_manifest(
        &masked_page,
        logs,
        sql_traces,
        "realtime",
        "realtime-package",
        Some(gateway_prefix),
    );
    build_realtime_package_with_manifest(
        logs,
        sql_traces,
        &[],
        explain_plans,
        table_stats,
        captured_page,
        gateway_prefix,
        &manifest,
        None,
        &privacy,
        None,
        output_path,
    )
}

pub fn build_quick_package_with_manifest(
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    slow_sqls: &[SlowSqlItem],
    explain_plans: &[ExplainPlan],
    table_stats: &[TableStats],
    manifest: &DiagnosisManifest,
    collection_report: Option<&CollectionReport>,
    masking_report: Option<&MaskingReport>,
    output_path: &Path,
) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = std::fs::File::create(output_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let captured_page = CapturedPage {
        page_url: manifest.page_url.clone(),
        requests: Vec::new(),
    };

    write_structured_contents(
        &mut zip,
        options,
        manifest,
        &captured_page,
        logs,
        sql_traces,
        slow_sqls,
        explain_plans,
        table_stats,
        collection_report,
        masking_report,
    )?;

    zip.start_file("diagnosis-report.md", options)?;
    zip.write_all(
        render_quick_unified_report_md(logs, sql_traces, explain_plans, table_stats, manifest)
            .as_bytes(),
    )?;

    zip.finish()?;
    Ok(())
}

pub fn build_realtime_package_with_manifest(
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    slow_sqls: &[SlowSqlItem],
    explain_plans: &[ExplainPlan],
    table_stats: &[TableStats],
    captured_page: &CapturedPage,
    gateway_prefix: &str,
    manifest: &DiagnosisManifest,
    collection_report: Option<&CollectionReport>,
    privacy: &PrivacyConfig,
    masking_report: Option<&MaskingReport>,
    output_path: &Path,
) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = std::fs::File::create(output_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let masked_page = masked_captured_page(captured_page, privacy);
    let mut manifest = manifest.clone();
    manifest.page_url = masked_page.page_url.clone();
    manifest.request_count = masked_page.requests.len();
    manifest.gateway_prefix = Some(gateway_prefix.to_string());

    write_structured_contents(
        &mut zip,
        options,
        &manifest,
        &masked_page,
        logs,
        sql_traces,
        slow_sqls,
        explain_plans,
        table_stats,
        collection_report,
        masking_report,
    )?;

    zip.start_file("diagnosis-report.md", options)?;
    zip.write_all(
        render_realtime_unified_report_md(
            &masked_page,
            logs,
            sql_traces,
            explain_plans,
            table_stats,
            gateway_prefix,
        )
        .as_bytes(),
    )?;

    zip.finish()?;
    Ok(())
}

fn render_realtime_overview_md(
    captured_page: &CapturedPage,
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    explain_plans: &[ExplainPlan],
    gateway_prefix: &str,
) -> String {
    let mut md = String::new();
    md.push_str("# 实时诊断报告\n\n");
    md.push_str(&format!("页面 URL：{}\n\n", captured_page.page_url));
    md.push_str("## 一、问题总览\n\n");
    md.push_str(
        "| # | 风险 | traceId | 接口 | 状态码 | 耗时 | 服务 | 日志信号 | SQL | EXPLAIN |\n",
    );
    md.push_str(
        "|---|------|---------|------|--------|------|------|----------|-----|---------|\n",
    );

    for (idx, req) in captured_page.requests.iter().enumerate() {
        let trace_id = req.trace_id.as_deref().filter(|id| !id.is_empty());
        let request_logs = logs_for_trace(logs, trace_id);
        let request_sql = sql_for_trace(sql_traces, trace_id);
        let request_plans = plans_for_sqls(explain_plans, &request_sql);
        let parsed = resolve_request(req, gateway_prefix);
        let risk = classify_request_risk(req, &request_logs, &request_sql, &request_plans);
        let signal = format_log_signal(&request_logs);
        let explain = format_explain_status(&request_plans);
        let trace_label = trace_id.unwrap_or("无 traceId");

        md.push_str(&format!(
            "| {} | {} | `{}` | {} | {} | {}ms | {} | {} | {} | {} |\n",
            idx + 1,
            risk,
            trace_label,
            parsed.api_path,
            req.status,
            req.duration_ms,
            parsed.service,
            signal,
            request_sql.len(),
            explain,
        ));
    }

    md
}

fn render_realtime_request_cards_md(
    captured_page: &CapturedPage,
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    explain_plans: &[ExplainPlan],
    table_stats: &[TableStats],
    gateway_prefix: &str,
) -> String {
    let mut md = String::new();
    md.push_str("# 实时请求排查卡片\n\n");
    md.push_str(&format!("页面 URL：{}\n\n", captured_page.page_url));

    let captured_trace_ids: HashSet<&str> = captured_page
        .requests
        .iter()
        .filter_map(|request| request.trace_id.as_deref())
        .filter(|id| !id.is_empty())
        .collect();

    for (idx, req) in captured_page.requests.iter().enumerate() {
        let trace_id = req.trace_id.as_deref().filter(|id| !id.is_empty());
        let request_logs = logs_for_trace(logs, trace_id);
        let request_sql = sql_for_trace(sql_traces, trace_id);
        let request_plans = plans_for_sqls(explain_plans, &request_sql);
        let parsed = resolve_request(req, gateway_prefix);
        let risk = classify_request_risk(req, &request_logs, &request_sql, &request_plans);
        let trace_label = trace_id.unwrap_or("无 traceId");

        md.push_str(&format!(
            "## {}. [{}] x-trace：`{}`\n\n",
            idx + 1,
            risk,
            trace_label,
        ));
        md.push_str(&render_request_card_meta(req, &parsed));
        md.push_str(&render_request_diagnosis_summary(
            req,
            &request_logs,
            &request_sql,
            &request_plans,
        ));
        md.push_str(&render_request_key_logs(trace_id, &request_logs));
        md.push_str(&render_request_sql_cards(
            &request_sql,
            &request_plans,
            table_stats,
        ));
        md.push_str(&render_request_evidence_links(&request_logs, &request_sql));
        md.push_str("---\n\n");
    }

    let unmatched: Vec<&LogEntry> = logs
        .iter()
        .filter(|entry| {
            entry
                .trace_id
                .as_deref()
                .map(|id| !captured_trace_ids.contains(id))
                .unwrap_or(true)
        })
        .collect();

    if !unmatched.is_empty() {
        md.push_str("## 未匹配浏览器请求的日志\n\n```text\n");
        for entry in sort_log_refs(unmatched) {
            md.push_str(&format_log_line(entry));
            md.push('\n');
        }
        md.push_str("```\n\n");
    }

    md
}

struct RequestTarget {
    service: String,
    api_path: String,
}

fn resolve_request(req: &CapturedRequest, gateway_prefix: &str) -> RequestTarget {
    match url_resolver::resolve_url(&req.url, gateway_prefix) {
        Ok(resolved) => RequestTarget {
            service: resolved.service,
            api_path: resolved.api_path,
        },
        Err(_) => RequestTarget {
            service: "unknown".to_string(),
            api_path: req.url.clone(),
        },
    }
}

fn render_request_card_meta(req: &CapturedRequest, target: &RequestTarget) -> String {
    let start_time = request_start_time(&req.timestamp, req.duration_ms);
    let end_time = request_end_time(&req.timestamp);
    format!(
        "### 请求信息\n\n| 项目 | 值 |\n|------|-----|\n| Request URL | `{}` |\n| Method / Status | {} / {} |\n| startTime | {} |\n| endTime | {} |\n| duration | {} ms |\n| 入口服务 | {} |\n\n",
        req.url,
        req.method,
        req.status,
        start_time,
        end_time,
        req.duration_ms,
        target.service,
    )
}

fn render_request_diagnosis_summary(
    req: &CapturedRequest,
    logs: &[&LogEntry],
    sql_traces: &[&SqlTrace],
    plans: &[&ExplainPlan],
) -> String {
    let conclusion = request_conclusion(req, logs);
    let evidence = request_evidence(req, logs, sql_traces, plans);
    let priority = request_priority(sql_traces, plans, logs);
    format!(
        "### 初步判断\n\n- 结论：{}\n- 主要证据：{}\n- 建议优先排查：{}\n\n",
        conclusion, evidence, priority
    )
}

fn request_conclusion(req: &CapturedRequest, logs: &[&LogEntry]) -> &'static str {
    if req.status >= 500
        || logs
            .iter()
            .any(|entry| entry.level.eq_ignore_ascii_case("ERROR"))
    {
        "接口异常"
    } else if req.duration_ms > 2000 {
        "慢请求"
    } else if req.status >= 400
        || logs
            .iter()
            .any(|entry| entry.level.eq_ignore_ascii_case("WARN"))
    {
        "日志告警"
    } else {
        "暂无明显异常"
    }
}

fn request_evidence(
    req: &CapturedRequest,
    logs: &[&LogEntry],
    sql_traces: &[&SqlTrace],
    plans: &[&ExplainPlan],
) -> String {
    let mut evidence = Vec::new();
    if req.status >= 500 {
        evidence.push(format!("HTTP {}", req.status));
    }
    if logs
        .iter()
        .any(|entry| entry.level.eq_ignore_ascii_case("ERROR"))
    {
        evidence.push("ERROR 日志".to_string());
    }
    if logs
        .iter()
        .any(|entry| entry.level.eq_ignore_ascii_case("WARN"))
    {
        evidence.push("WARN 日志".to_string());
    }
    if req.duration_ms > 2000 {
        evidence.push("慢请求".to_string());
    }
    if sql_traces.iter().any(|trace| {
        trace
            .duration_ms
            .map(|duration| duration > 1000.0)
            .unwrap_or(false)
    }) {
        evidence.push("慢 SQL".to_string());
    }
    if plans.iter().any(|plan| plan.error.is_some()) {
        evidence.push("EXPLAIN 失败".to_string());
    }
    if evidence.is_empty() {
        "未发现明显异常信号".to_string()
    } else {
        evidence.join("、")
    }
}

fn request_priority(
    sql_traces: &[&SqlTrace],
    plans: &[&ExplainPlan],
    logs: &[&LogEntry],
) -> String {
    if plans.iter().any(|plan| plan.error.is_some()) {
        "SQL 执行计划失败原因".to_string()
    } else if !sql_traces.is_empty() {
        "相关 SQL、表数据量与索引".to_string()
    } else if logs
        .iter()
        .any(|entry| entry.level.eq_ignore_ascii_case("ERROR"))
    {
        "异常日志和堆栈".to_string()
    } else {
        "请求状态、耗时和关联服务日志".to_string()
    }
}

fn logs_for_trace<'a>(logs: &'a [LogEntry], trace_id: Option<&str>) -> Vec<&'a LogEntry> {
    match trace_id {
        Some(id) => logs
            .iter()
            .filter(|entry| entry.trace_id.as_deref() == Some(id))
            .collect(),
        None => Vec::new(),
    }
}

fn sql_for_trace<'a>(sql_traces: &'a [SqlTrace], trace_id: Option<&str>) -> Vec<&'a SqlTrace> {
    match trace_id {
        Some(id) => sql_traces
            .iter()
            .filter(|trace| trace.trace_id == id)
            .collect(),
        None => Vec::new(),
    }
}

fn plans_for_sqls<'a>(
    explain_plans: &'a [ExplainPlan],
    sql_traces: &[&SqlTrace],
) -> Vec<&'a ExplainPlan> {
    explain_plans
        .iter()
        .filter(|plan| {
            sql_traces
                .iter()
                .any(|trace| trace.sql_fingerprint == plan.sql_fingerprint)
        })
        .collect()
}

fn render_request_key_logs(trace_id: Option<&str>, logs: &[&LogEntry]) -> String {
    let mut md = String::new();
    md.push_str("### 关键日志\n\n");
    if trace_id.is_none() {
        md.push_str("> 无 traceId，无法关联日志\n\n");
        return md;
    }
    if logs.is_empty() {
        md.push_str("> 未查询到该 traceId 的日志\n\n");
        return md;
    }

    let key_logs: Vec<&LogEntry> = logs
        .iter()
        .copied()
        .filter(|entry| is_key_log(entry))
        .collect();
    let display_logs = if key_logs.is_empty() {
        sort_log_refs(logs.to_vec())
    } else {
        sort_log_refs(key_logs)
    };

    md.push_str("```text\n");
    for entry in display_logs {
        md.push_str(&format_log_line(entry));
        md.push('\n');
        if let Some(stack) = entry
            .stack_trace
            .as_deref()
            .filter(|stack| !stack.trim().is_empty())
        {
            md.push_str(stack);
            md.push('\n');
        }
    }
    md.push_str("```\n\n");
    md
}

fn is_key_log(entry: &LogEntry) -> bool {
    let level = entry.level.as_str();
    let msg = entry.message.as_str();
    level.eq_ignore_ascii_case("ERROR")
        || level.eq_ignore_ascii_case("WARN")
        || entry.exception.is_some()
        || entry.stack_trace.is_some()
        || msg.contains("RequestUrl:")
        || msg.contains("==>  Preparing:")
        || msg.contains("==> Preparing:")
        || msg.contains("==> Parameters:")
        || msg.contains("<==      Total:")
}

fn sort_log_refs(mut logs: Vec<&LogEntry>) -> Vec<&LogEntry> {
    logs.sort_by(|a, b| {
        a.time
            .as_deref()
            .unwrap_or("")
            .cmp(b.time.as_deref().unwrap_or(""))
            .then_with(|| a.service.cmp(&b.service))
    });
    logs
}

fn render_request_sql_cards(
    sql_traces: &[&SqlTrace],
    plans: &[&ExplainPlan],
    table_stats: &[TableStats],
) -> String {
    let mut md = String::new();
    md.push_str("### 相关 SQL 与执行计划\n\n");
    if sql_traces.is_empty() {
        md.push_str("> 未匹配到该请求的 SQL\n\n");
        return md;
    }

    let mut stats_map: HashMap<&str, Vec<&TableStats>> = HashMap::new();
    for stat in table_stats {
        stats_map
            .entry(stat.table_name.as_str())
            .or_default()
            .push(stat);
    }

    for (idx, trace) in sql_traces.iter().enumerate() {
        let trace_plans: Vec<&ExplainPlan> = plans
            .iter()
            .copied()
            .filter(|plan| plan.sql_fingerprint == trace.sql_fingerprint)
            .collect();
        md.push_str(&render_request_sql_card(
            idx + 1,
            trace,
            &trace_plans,
            &stats_map,
        ));
    }

    md
}

fn render_request_sql_card(
    idx: usize,
    trace: &SqlTrace,
    plans: &[&ExplainPlan],
    stats_map: &HashMap<&str, Vec<&TableStats>>,
) -> String {
    let title = trace
        .tables
        .first()
        .map(|table| display_table_name(table, stats_map))
        .unwrap_or_else(|| "SQL 查询".to_string());
    let executed_sql = executable_sql_for_trace(trace, plans);
    let explain_status = if plans.iter().any(|plan| plan.error.is_some()) {
        "失败"
    } else if plans.is_empty() {
        "无"
    } else {
        "成功"
    };

    let mut md = String::new();
    md.push_str(&format!("#### SQL {}：{}\n\n", idx, title));
    md.push_str("| 项目 | 值 |\n");
    md.push_str("|------|-----|\n");
    md.push_str(&format!("| traceId | `{}` |\n", trace.trace_id));
    md.push_str(&format!("| 服务 | {} |\n", trace.service));
    if let Some(ts) = &trace.timestamp {
        md.push_str(&format!("| 时间 | {} |\n", ts));
    }
    if let Some(duration) = trace.duration_ms {
        md.push_str(&format!("| 耗时 | {:.2} ms |\n", duration));
    }
    if !trace.tables.is_empty() {
        let tables: Vec<String> = trace
            .tables
            .iter()
            .map(|table| display_table_name(table, stats_map))
            .collect();
        md.push_str(&format!("| 涉及表 | {} |\n", tables.join(", ")));
    }
    md.push_str(&format!(
        "| 参数状态 | {} |\n",
        parameter_status(trace, plans)
    ));
    md.push_str(&format!("| EXPLAIN 状态 | {} |\n\n", explain_status));

    md.push_str("```sql\n");
    md.push_str(&executed_sql);
    md.push_str("\n```\n\n");

    md.push_str(&render_request_table_stats(&trace.tables, stats_map));
    md.push_str(&render_request_explain_plans(plans));
    md
}

fn parameter_status(trace: &SqlTrace, plans: &[&ExplainPlan]) -> &'static str {
    if plans.iter().any(|plan| {
        plan.error
            .as_deref()
            .map(|error| error.contains("参数未完整拼装") || error.contains("? 占位符"))
            .unwrap_or(false)
    }) {
        "参数缺失"
    } else if trace
        .parameters
        .as_deref()
        .map(|params| !params.trim().is_empty())
        .unwrap_or(false)
        || trace_specific_executed_sql(trace, plans).is_some()
    {
        "已拼装"
    } else {
        "参数缺失"
    }
}

fn display_table_name(table: &str, stats_map: &HashMap<&str, Vec<&TableStats>>) -> String {
    match stats_map.get(table).and_then(|stats| stats.first()) {
        Some(stats) if !stats.schema.is_empty() => format!("{}.{}", stats.schema, table),
        _ => table.to_string(),
    }
}

fn render_request_table_stats(
    tables: &[String],
    stats_map: &HashMap<&str, Vec<&TableStats>>,
) -> String {
    if tables.is_empty() {
        return String::new();
    }

    let mut md = String::new();
    md.push_str("**表数据量与索引：**\n\n");
    md.push_str("| 表名 | 行数 | 数据大小 | 索引数 | 索引列表 |\n");
    md.push_str("|------|------|----------|--------|----------|\n");
    for table in tables {
        if let Some(stats_list) = stats_map.get(table.as_str()) {
            for stats in stats_list {
                let idx_list: Vec<String> = stats
                    .indexes
                    .iter()
                    .map(|index| {
                        let unique = if index.unique { " UNIQUE" } else { "" };
                        format!("`{}({}){}`", index.name, index.columns.join(","), unique)
                    })
                    .collect();
                let table_display = if stats.schema.is_empty() {
                    table.clone()
                } else {
                    format!("{}.{}", stats.schema, table)
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    table_display,
                    format_number(stats.row_count),
                    stats
                        .data_size_bytes
                        .map(|size| format!("{} bytes", format_number(size)))
                        .unwrap_or_else(|| "-".to_string()),
                    stats.indexes.len(),
                    if idx_list.is_empty() {
                        "-".to_string()
                    } else {
                        idx_list.join("<br>")
                    },
                ));
            }
        } else {
            md.push_str(&format!("| {} | - | - | - | - |\n", table));
        }
    }
    md.push('\n');
    md
}

fn render_request_explain_plans(plans: &[&ExplainPlan]) -> String {
    if plans.is_empty() {
        return "**执行计划：** 无（未匹配到 EXPLAIN 结果）\n\n".to_string();
    }

    let mut md = String::new();
    for plan in plans {
        let source_suffix = if let Some(schema) = &plan.found_in_schema {
            format!("{} - 来自 schema: {}", plan.source, schema)
        } else {
            plan.source.clone()
        };
        md.push_str(&format!("**执行计划（{}）：**\n\n", source_suffix));
        if let Some(err) = &plan.error {
            md.push_str(&format!("> ⚠ EXPLAIN 执行失败：`{}`\n\n", err));
        } else {
            md.push_str(&render_explain_plan_md(plan));
            md.push('\n');
        }
    }
    md
}

fn render_request_evidence_links(logs: &[&LogEntry], sql_traces: &[&SqlTrace]) -> String {
    let mut log_services: Vec<&str> = logs.iter().map(|entry| entry.service.as_str()).collect();
    log_services.sort_unstable();
    log_services.dedup();

    let mut sql_services: Vec<&str> = sql_traces
        .iter()
        .map(|trace| trace.service.as_str())
        .collect();
    sql_services.sort_unstable();
    sql_services.dedup();

    let mut md = String::new();
    md.push_str("### 关联证据\n\n");
    if log_services.is_empty() && sql_services.is_empty() {
        md.push_str("- 未关联到服务级证据\n\n");
        return md;
    }

    if !log_services.is_empty() {
        md.push_str(&format!("- 关联日志服务：{}\n", log_services.join("、")));
    }
    if !sql_services.is_empty() {
        md.push_str(&format!("- 关联 SQL 服务：{}\n", sql_services.join("、")));
    }
    md.push('\n');
    md
}

fn classify_request_risk(
    req: &CapturedRequest,
    logs: &[&LogEntry],
    sql_traces: &[&SqlTrace],
    plans: &[&ExplainPlan],
) -> &'static str {
    let has_error_log = logs
        .iter()
        .any(|entry| entry.level.eq_ignore_ascii_case("ERROR"));
    let has_warn_log = logs
        .iter()
        .any(|entry| entry.level.eq_ignore_ascii_case("WARN"));
    let has_slow_sql = sql_traces
        .iter()
        .any(|trace| trace.duration_ms.map(|d| d > 1000.0).unwrap_or(false));
    let has_explain_error = plans.iter().any(|plan| plan.error.is_some());

    if req.status >= 500 || has_error_log {
        "ERROR"
    } else if req.duration_ms > 2000 || has_slow_sql {
        "SLOW"
    } else if req.status >= 400 || req.duration_ms > 1000 || has_warn_log || has_explain_error {
        "WARN"
    } else {
        "OK"
    }
}

fn format_log_signal(logs: &[&LogEntry]) -> String {
    let error_count = logs
        .iter()
        .filter(|entry| entry.level.eq_ignore_ascii_case("ERROR"))
        .count();
    let warn_count = logs
        .iter()
        .filter(|entry| entry.level.eq_ignore_ascii_case("WARN"))
        .count();
    format!("ERROR={} WARN={}", error_count, warn_count)
}

fn format_explain_status(plans: &[&ExplainPlan]) -> String {
    let success = plans.iter().filter(|plan| plan.error.is_none()).count();
    let failed = plans.iter().filter(|plan| plan.error.is_some()).count();
    format!("{} 成功 / {} 失败", success, failed)
}

fn trace_specific_executed_sql(trace: &SqlTrace, plans: &[&ExplainPlan]) -> Option<String> {
    plans
        .iter()
        .find(|plan| plan.trace_id.as_deref() == Some(trace.trace_id.as_str()))
        .and_then(|plan| plan.executed_sql.clone())
        .or_else(|| {
            plans
                .iter()
                .find(|plan| plan.trace_id.is_none())
                .and_then(|plan| plan.executed_sql.clone())
        })
}

fn executable_sql_for_trace(trace: &SqlTrace, plans: &[&ExplainPlan]) -> String {
    trace_specific_executed_sql(trace, plans).unwrap_or_else(|| match &trace.parameters {
        Some(parameters) if !parameters.trim().is_empty() => {
            crate::sql_parser::substitute_mybatis_parameters(&trace.sql, parameters)
        }
        _ => trace.sql.clone(),
    })
}

/// 实时模式统一报告：将总览表格 + 单请求排查卡片合并为一个 Markdown
fn render_realtime_unified_report_md(
    captured_page: &CapturedPage,
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    explain_plans: &[ExplainPlan],
    table_stats: &[TableStats],
    gateway_prefix: &str,
) -> String {
    let mut md = String::new();

    // Part 1: 总览表格
    md.push_str(&render_realtime_overview_md(
        captured_page,
        logs,
        sql_traces,
        explain_plans,
        gateway_prefix,
    ));

    md.push_str("\n---\n\n");

    // Part 2: 排查卡片（含日志、SQL、EXPLAIN、表统计）
    md.push_str(&render_realtime_request_cards_md(
        captured_page,
        logs,
        sql_traces,
        explain_plans,
        table_stats,
        gateway_prefix,
    ));

    md
}

/// 历史/快速模式统一报告：按 traceId 分组展示日志 + SQL + EXPLAIN
fn render_quick_unified_report_md(
    logs: &[LogEntry],
    sql_traces: &[SqlTrace],
    explain_plans: &[ExplainPlan],
    table_stats: &[TableStats],
    manifest: &DiagnosisManifest,
) -> String {
    let mut md = String::new();
    md.push_str("# 诊断报告\n\n");
    md.push_str(&format!("诊断 ID：{}\n\n", manifest.diagnosis_id));
    if let Some(ref time_range) = manifest.time_range {
        md.push_str(&format!(
            "时间范围：{} ~ {}\n\n",
            time_range.start, time_range.end
        ));
    }

    // 构建索引
    let mut stats_map: HashMap<&str, Vec<&TableStats>> = HashMap::new();
    for stat in table_stats {
        stats_map
            .entry(stat.table_name.as_str())
            .or_default()
            .push(stat);
    }
    let mut explain_map: HashMap<&str, Vec<&ExplainPlan>> = HashMap::new();
    for ep in explain_plans {
        explain_map
            .entry(ep.sql_fingerprint.as_str())
            .or_default()
            .push(ep);
    }

    // 按 traceId 分组日志
    let mut logs_by_trace: HashMap<&str, Vec<&LogEntry>> = HashMap::new();
    for entry in logs {
        if let Some(trace_id) = entry.trace_id.as_deref().filter(|id| !id.is_empty()) {
            logs_by_trace.entry(trace_id).or_default().push(entry);
        }
    }

    // 按 traceId 分组 SQL
    let mut sql_by_trace: HashMap<&str, Vec<&SqlTrace>> = HashMap::new();
    for trace in sql_traces {
        if !trace.trace_id.is_empty() {
            sql_by_trace
                .entry(trace.trace_id.as_str())
                .or_default()
                .push(trace);
        }
    }

    // 收集所有 traceId
    let mut all_trace_ids: Vec<&str> = logs_by_trace
        .keys()
        .chain(sql_by_trace.keys())
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    all_trace_ids.sort();

    if all_trace_ids.is_empty() {
        md.push_str("> 未采集到任何 traceId 关联的日志或 SQL\n\n");
        return md;
    }

    // 总览
    md.push_str("## 一、总览\n\n");
    md.push_str("| # | traceId | 日志条数 | SQL 条数 |\n");
    md.push_str("|---|---------|----------|----------|\n");
    for (idx, trace_id) in all_trace_ids.iter().enumerate() {
        let log_count = logs_by_trace.get(trace_id).map(|v| v.len()).unwrap_or(0);
        let sql_count = sql_by_trace.get(trace_id).map(|v| v.len()).unwrap_or(0);
        md.push_str(&format!(
            "| {} | `{}` | {} | {} |\n",
            idx + 1,
            trace_id,
            log_count,
            sql_count,
        ));
    }
    md.push_str("\n---\n\n");

    // 每个 traceId 一个排查卡片
    md.push_str("## 二、排查详情\n\n");
    for (idx, trace_id) in all_trace_ids.iter().enumerate() {
        md.push_str(&format!("### {}. traceId：`{}`\n\n", idx + 1, trace_id));

        // 日志
        md.push_str("#### 关键日志\n\n");
        if let Some(entries) = logs_by_trace.get(trace_id) {
            let key_logs: Vec<&LogEntry> = entries
                .iter()
                .copied()
                .filter(|entry| is_key_log(entry))
                .collect();
            let display = if key_logs.is_empty() {
                sort_log_refs(entries.to_vec())
            } else {
                sort_log_refs(key_logs)
            };
            md.push_str("```text\n");
            for entry in display {
                md.push_str(&format_log_line(entry));
                md.push('\n');
                if let Some(stack) = entry
                    .stack_trace
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                {
                    md.push_str(stack);
                    md.push('\n');
                }
            }
            md.push_str("```\n\n");
        } else {
            md.push_str("> 未查询到该 traceId 的日志\n\n");
        }

        // SQL
        if let Some(traces) = sql_by_trace.get(trace_id) {
            md.push_str("#### 相关 SQL 与执行计划\n\n");
            for (sql_idx, trace) in traces.iter().enumerate() {
                let section =
                    render_sql_trace_md(sql_idx + 1, trace, &stats_map, &explain_map);
                md.push_str(&section);
            }
        }

        md.push_str("---\n\n");
    }

    // 未关联 traceId 的日志
    let unmatched: Vec<&LogEntry> = logs
        .iter()
        .filter(|entry| {
            entry
                .trace_id
                .as_deref()
                .map(|id| id.is_empty())
                .unwrap_or(true)
        })
        .collect();
    if !unmatched.is_empty() {
        md.push_str("## 三、未关联 traceId 的日志\n\n```text\n");
        for entry in sort_log_refs(unmatched) {
            md.push_str(&format_log_line(entry));
            md.push('\n');
        }
        md.push_str("```\n\n");
    }

    md
}


fn request_start_time(completed_at: &str, duration_ms: u64) -> String {
    DateTime::parse_from_rfc3339(completed_at)
        .map(|dt: DateTime<FixedOffset>| {
            (dt - Duration::milliseconds(duration_ms.min(i64::MAX as u64) as i64)).to_rfc3339()
        })
        .unwrap_or_else(|_| "-".to_string())
}

fn request_end_time(completed_at: &str) -> String {
    DateTime::parse_from_rfc3339(completed_at)
        .map(|dt: DateTime<FixedOffset>| dt.to_rfc3339())
        .unwrap_or_else(|_| {
            if completed_at.is_empty() {
                "-".to_string()
            } else {
                completed_at.to_string()
            }
        })
}

/// 渲染单条 SQL trace 为 Markdown 段落
fn render_sql_trace_md(
    idx: usize,
    trace: &SqlTrace,
    stats_map: &HashMap<&str, Vec<&TableStats>>,
    explain_map: &HashMap<&str, Vec<&ExplainPlan>>,
) -> String {
    let mut md = String::new();

    // 标题：使用第一个表名作为主题
    let topic = trace
        .tables
        .first()
        .cloned()
        .unwrap_or_else(|| "SQL 查询".to_string());
    md.push_str(&format!("## {}. {}\n\n", idx, topic));

    // 元信息表
    md.push_str("| 项目 | 值 |\n");
    md.push_str("|------|-----|\n");
    md.push_str(&format!("| traceId | `{}` |\n", trace.trace_id));
    md.push_str(&format!("| 服务 | {} |\n", trace.service));
    if let Some(ts) = &trace.timestamp {
        md.push_str(&format!("| 时间 | {} |\n", ts));
    }
    if let Some(d) = trace.duration_ms {
        md.push_str(&format!("| 耗时 | {:.2} ms |\n", d));
    }
    if let Some(p) = &trace.parameters {
        md.push_str(&format!("| 参数 | `{}` |\n", p.replace('|', "\\|")));
    }
    if !trace.tables.is_empty() {
        md.push_str(&format!("| 涉及表 | {} |\n", trace.tables.join(", ")));
    }
    md.push('\n');

    // 拼装后的可执行 SQL（优先取 explain_map 中的 executed_sql；否则用 trace.sql 自行拼装）
    let executed = explain_map
        .get(trace.sql_fingerprint.as_str())
        .map(|plans| executable_sql_for_trace(trace, plans.as_slice()))
        .unwrap_or_else(|| executable_sql_for_trace(trace, &[]));

    md.push_str("**SQL 语句（已拼装参数）：**\n\n```sql\n");
    md.push_str(&executed);
    md.push_str("\n```\n\n");

    if !trace.tables.is_empty() {
        md.push_str("**表数据量与索引：**\n\n");
        md.push_str("| 表名 | 行数 | 数据大小 | 索引数 | 索引列表 |\n");
        md.push_str("|------|------|----------|--------|----------|\n");
        for table in &trace.tables {
            if let Some(stats_list) = stats_map.get(table.as_str()) {
                for stats in stats_list {
                    let idx_list: Vec<String> = stats
                        .indexes
                        .iter()
                        .map(|i| {
                            let unique = if i.unique { " UNIQUE" } else { "" };
                            format!("`{}({}){}`", i.name, i.columns.join(","), unique)
                        })
                        .collect();
                    let table_display = if stats.schema.is_empty() {
                        table.clone()
                    } else {
                        format!("{}.{}", stats.schema, table)
                    };
                    md.push_str(&format!(
                        "| {} | {} | {} | {} | {} |\n",
                        table_display,
                        format_number(stats.row_count),
                        stats
                            .data_size_bytes
                            .map(|b| format!("{} bytes", format_number(b)))
                            .unwrap_or_else(|| "-".to_string()),
                        stats.indexes.len(),
                        if idx_list.is_empty() {
                            "-".to_string()
                        } else {
                            idx_list.join("<br>")
                        },
                    ));
                }
            } else {
                md.push_str(&format!("| {} | - | - | - | - |\n", table));
            }
        }
        md.push('\n');
    }

    // 执行计划
    if let Some(plans) = explain_map.get(trace.sql_fingerprint.as_str()) {
        for plan in plans {
            let source_suffix = if let Some(ref schema) = plan.found_in_schema {
                format!("{} - 来自 schema: {}", plan.source, schema)
            } else {
                plan.source.clone()
            };
            md.push_str(&format!("**执行计划（{}）：**\n\n", source_suffix));
            if let Some(err) = &plan.error {
                md.push_str(&format!("> ⚠ EXPLAIN 执行失败：`{}`\n\n", err));
                continue;
            }
            md.push_str(&render_explain_plan_md(plan));
            md.push('\n');
        }
    } else {
        md.push_str("**执行计划：** 无（未匹配到 EXPLAIN 结果）\n\n");
    }

    md.push_str("---\n\n");
    md
}

fn render_explain_plan_md(plan: &ExplainPlan) -> String {
    if plan.explain_rows.is_empty() {
        return "无 EXPLAIN 行\n".to_string();
    }

    let first = &plan.explain_rows[0];
    // PG 格式（JSON 字符串）
    if let Some(extra) = &first.extra {
        let trimmed = extra.trim_start();
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            return format!("```json\n{}\n```\n", extra);
        }
    }

    // MySQL 表格
    let mut md = String::new();
    md.push_str(
        "| id | select_type | table | type | possible_keys | key | rows | filtered | Extra |\n",
    );
    md.push_str(
        "|----|-------------|-------|------|---------------|-----|------|----------|-------|\n",
    );
    for row in &plan.explain_rows {
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            opt_str(&row.id),
            opt_string(&row.select_type),
            opt_string(&row.table),
            opt_string(&row.access_type),
            opt_string(&row.possible_keys),
            opt_string(&row.key),
            opt_str(&row.rows),
            row.filtered
                .map(|f| format!("{:.1}", f))
                .unwrap_or_else(|| "-".into()),
            opt_string(&row.extra).replace('|', "\\|"),
        ));
    }
    md
}

fn opt_str<T: std::fmt::Display>(v: &Option<T>) -> String {
    v.as_ref()
        .map(|x| x.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn opt_string(v: &Option<String>) -> String {
    v.clone().unwrap_or_else(|| "-".to_string())
}

fn format_number(n: i64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn format_log_line(entry: &LogEntry) -> String {
    let time = entry.time.as_deref().unwrap_or("-");
    let trace_id = entry.trace_id.as_deref().unwrap_or("-");
    let thread = entry.thread.as_deref().unwrap_or("-");
    format!(
        "[{}] [{}] [{}] [{}] [{}] {}",
        time, entry.level, trace_id, entry.service, thread, entry.message
    )
}
