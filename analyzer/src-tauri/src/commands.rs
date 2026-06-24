use diag_core::models::{DiagnosisPackage, Finding};
use diag_core::url_resolver;
use serde::{Deserialize, Serialize};

use crate::report;
use crate::rule_engine;

/// 导入分析结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisResult {
    pub manifest: serde_json::Value,
    pub request_summary: Vec<RequestSummary>,
    pub log_summary: LogSummary,
    pub sql_summary: Vec<SqlSummary>,
    pub sql_trace_summary: Vec<SqlTraceSummary>,
    pub findings: Vec<Finding>,
    pub report_markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestSummary {
    pub method: String,
    pub url: String,
    pub service: String,
    pub api_path: String,
    pub status: u16,
    pub duration_ms: u64,
    pub trace_id: Option<String>,
    pub risk_level: String, // "OK" | "SLOW" | "ERROR"
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LogSummary {
    pub total_lines: usize,
    pub error_count: usize,
    pub warn_count: usize,
    pub exception_classes: Vec<String>,
    pub error_services: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlSummary {
    pub sql_fingerprint: String,
    pub duration_ms: f64,
    pub tables: Vec<String>,
    pub rows_examined: Option<i64>,
    pub rows_returned: Option<i64>,
    pub risk_level: String,
    pub risk_reasons: Vec<String>,
}

/// 从日志中提取的 SQL（按 traceId 关联，比 performance_schema 更精准）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlTraceSummary {
    pub trace_id: String,
    pub service: String,
    pub sql_fingerprint: String,
    pub tables: Vec<String>,
    pub count: usize, // 同一 traceId 内的 SQL 条数
}

/// 导入诊断包
#[tauri::command]
pub fn import_diagnosis_package(zip_path: String) -> Result<serde_json::Value, String> {
    let path = std::path::Path::new(&zip_path);
    if !path.exists() {
        return Err(format!("文件不存在: {}", zip_path));
    }

    let package =
        diag_core::package::read_package(path).map_err(|e| format!("导入失败: {}", e))?;

    // 返回 manifest 概要
    let summary = serde_json::json!({
        "diagnosisId": package.manifest.diagnosis_id,
        "site": package.manifest.site,
        "createdAt": package.manifest.created_at,
        "pageUrl": package.manifest.page_url,
        "requestCount": package.manifest.request_count,
        "services": package.manifest.services,
        "traceIds": package.manifest.trace_ids,
        "databaseType": package.manifest.database_type,
        "logCount": package.logs.len(),
        "slowSqlCount": package.slow_sqls.len(),
    });

    Ok(summary)
}

/// 执行完整分析
#[tauri::command]
pub fn run_analysis(zip_path: String) -> Result<AnalysisResult, String> {
    let path = std::path::Path::new(&zip_path);
    let package =
        diag_core::package::read_package(path).map_err(|e| format!("导入失败: {}", e))?;

    // 1. 请求分析
    let request_summary = analyze_requests(&package);

    // 2. 日志分析
    let log_summary = analyze_logs(&package);

    // 3. SQL 分析
    let sql_summary = analyze_sql(&package);

    // 3.5 SQL Trace 分析（按 traceId 关联，比 performance_schema 更精准）
    let sql_trace_summary = analyze_sql_traces(&package);

    // 4. 规则引擎
    let findings = rule_engine::diagnose(
        &package,
        &request_summary,
        &log_summary,
        &sql_summary,
        &sql_trace_summary,
    );

    // 5. 生成报告
    let report_markdown = report::generate_markdown(
        &package,
        &request_summary,
        &log_summary,
        &sql_summary,
        &sql_trace_summary,
        &findings,
    );

    let manifest = serde_json::to_value(&package.manifest).unwrap_or_default();

    Ok(AnalysisResult {
        manifest,
        request_summary,
        log_summary,
        sql_summary,
        sql_trace_summary,
        findings,
        report_markdown,
    })
}

/// 导出报告
#[tauri::command]
pub fn export_report(report_content: String, output_path: String) -> Result<String, String> {
    std::fs::write(&output_path, &report_content).map_err(|e| format!("导出失败: {}", e))?;
    Ok(format!("报告已导出到: {}", output_path))
}

// ─── 内部分析函数 ───

fn analyze_requests(package: &DiagnosisPackage) -> Vec<RequestSummary> {
    let gateway_prefix = package.manifest.gateway_prefix
        .as_deref()
        .unwrap_or("/gateway");

    package
        .captured_page
        .requests
        .iter()
        .map(|req| {
            let (service, api_path) = match url_resolver::resolve_url(&req.url, gateway_prefix) {
                Ok(r) => (r.service, r.api_path),
                Err(_) => ("unknown".to_string(), req.url.clone()),
            };

            let risk_level = if req.status >= 500 {
                "ERROR"
            } else if req.status >= 400 {
                "WARN"
            } else if req.duration_ms > 2000 {
                "SLOW"
            } else if req.duration_ms > 1000 {
                "WARN"
            } else {
                "OK"
            };

            RequestSummary {
                method: req.method.clone(),
                url: req.url.clone(),
                service,
                api_path,
                status: req.status,
                duration_ms: req.duration_ms,
                trace_id: req.trace_id.clone(),
                risk_level: risk_level.to_string(),
            }
        })
        .collect()
}

fn analyze_logs(package: &DiagnosisPackage) -> LogSummary {
    let mut summary = LogSummary {
        total_lines: package.logs.len(),
        ..Default::default()
    };

    let mut exception_set = std::collections::HashSet::new();
    let mut error_service_set = std::collections::HashSet::new();

    for log in &package.logs {
        match log.level.as_str() {
            "ERROR" => {
                summary.error_count += 1;
                error_service_set.insert(log.service.clone());
                if let Some(exc) = &log.exception {
                    exception_set.insert(exc.clone());
                }
            }
            "WARN" => summary.warn_count += 1,
            _ => {}
        }
    }

    summary.exception_classes = exception_set.into_iter().collect();
    summary.error_services = error_service_set.into_iter().collect();
    summary
}

fn analyze_sql_traces(package: &DiagnosisPackage) -> Vec<SqlTraceSummary> {
    use std::collections::HashMap;

    // 按 traceId 聚合
    let mut by_trace: HashMap<String, Vec<&diag_core::models::SqlTrace>> = HashMap::new();
    for trace in &package.sql_traces {
        by_trace.entry(trace.trace_id.clone()).or_default().push(trace);
    }

    by_trace
        .into_iter()
        .map(|(trace_id, traces)| {
            let first = traces[0];
            SqlTraceSummary {
                trace_id,
                service: first.service.clone(),
                sql_fingerprint: first.sql_fingerprint.clone(),
                tables: first.tables.clone(),
                count: traces.len(),
            }
        })
        .collect()
}

fn analyze_sql(package: &DiagnosisPackage) -> Vec<SqlSummary> {
    package
        .slow_sqls
        .iter()
        .map(|sql| {
            let mut risk_reasons = Vec::new();
            let mut risk_level = "LOW";

            if sql.duration_ms > 1000.0 {
                risk_reasons.push(format!("SQL 耗时 {:.0}ms > 1000ms", sql.duration_ms));
                risk_level = "HIGH";
            }

            if let (Some(examined), Some(returned)) = (sql.rows_examined, sql.rows_returned) {
                if returned > 0 && examined / returned > 100 {
                    risk_reasons.push(format!(
                        "扫描放大: 检查 {} 行 / 返回 {} 行 = {}x",
                        examined,
                        returned,
                        examined / returned
                    ));
                    risk_level = "HIGH";
                }
            }

            if let Some(ref explain) = sql.explain_summary {
                if explain.access_type.as_deref() == Some("ALL") {
                    risk_reasons.push("全表扫描 (type=ALL)".to_string());
                    risk_level = "HIGH";
                }
                if explain.extra.iter().any(|e| e.contains("filesort")) {
                    risk_reasons.push("使用文件排序 (Using filesort)".to_string());
                }
                if explain.extra.iter().any(|e| e.contains("temporary")) {
                    risk_reasons.push("使用临时表 (Using temporary)".to_string());
                }
                if explain.key_used.is_none() {
                    risk_reasons.push("未使用索引".to_string());
                    risk_level = "HIGH";
                }
            }

            SqlSummary {
                sql_fingerprint: sql.sql_fingerprint.clone(),
                duration_ms: sql.duration_ms,
                tables: sql.tables.clone(),
                rows_examined: sql.rows_examined,
                rows_returned: sql.rows_returned,
                risk_level: risk_level.to_string(),
                risk_reasons,
            }
        })
        .collect()
}
