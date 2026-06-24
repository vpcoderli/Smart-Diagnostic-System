use diag_core::models::{DiagnosisPackage, Finding, FindingType, Severity};
use crate::commands::{LogSummary, RequestSummary, SqlSummary, SqlTraceSummary};

/// 基于证据的规则引擎，输出根因候选
pub fn diagnose(
    _package: &DiagnosisPackage,
    requests: &[RequestSummary],
    logs: &LogSummary,
    _sqls: &[SqlSummary],
    sql_traces: &[SqlTraceSummary],
) -> Vec<Finding> {
    let mut findings = Vec::new();

    // ─── 规则 1: 慢请求 + 同 traceId 有 SQL（真正关联，非笛卡尔积）───
    for req in requests {
        if req.duration_ms <= 1000 { continue; }
        if req.trace_id.is_none() { continue; }

        let req_trace = req.trace_id.as_deref().unwrap();
        let related_sqls: Vec<&SqlTraceSummary> = sql_traces
            .iter()
            .filter(|s| s.trace_id == req_trace)
            .collect();

        if related_sqls.is_empty() { continue; }

        let total_sql_count: usize = related_sqls.iter().map(|s| s.count).sum();
        let tables: Vec<String> = related_sqls.iter()
            .flat_map(|s| s.tables.iter().cloned())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        findings.push(Finding {
            finding_type: FindingType::SlowSql,
            severity: if req.duration_ms > 2000 { Severity::High } else { Severity::Medium },
            summary: format!(
                "接口 {} ({}) 耗时 {}ms，该请求内包含 {} 条 SQL（共涉及表: {}）",
                req.api_path, req.service, req.duration_ms,
                total_sql_count,
                if tables.is_empty() { "未知".to_string() } else { tables.join(", ") }
            ),
            evidence: {
                let mut ev = vec![
                    format!("接口耗时: {}ms", req.duration_ms),
                    format!("traceId: {}", req_trace),
                    format!("关联 SQL 条数: {}", total_sql_count),
                ];
                for s in related_sqls.iter().take(3) {
                    ev.push(format!("SQL: {}", s.sql_fingerprint));
                }
                ev
            },
            short_term: vec![
                "检查该接口的 SQL 执行计划（EXPLAIN）".into(),
                "确认涉及表的索引覆盖情况".into(),
                "限制页面大小上限（建议 ≤ 50）".into(),
            ],
            mid_term: vec![
                "对高频查询增加组合索引".into(),
                "重构动态查询避免全表扫描".into(),
            ],
            long_term: vec![
                "建立接口 SLO 与慢接口巡检".into(),
                "建立按医院数据量的性能基线".into(),
            ],
        });
    }

    // ─── 规则 2: 后端异常 ───
    if logs.error_count > 0 {
        // 按异常类聚合，避免对每个 exception_class 重复
        let mut all_classes = logs.exception_classes.clone();
        all_classes.sort();
        all_classes.dedup();

        if !all_classes.is_empty() {
            let (severity, short_term) = classify_exception_list(&all_classes);
            findings.push(Finding {
                finding_type: FindingType::BackendException,
                severity,
                summary: format!(
                    "服务 {} 抛出 {} 种异常，共 {} 条 ERROR",
                    logs.error_services.join("、"),
                    all_classes.len(),
                    logs.error_count
                ),
                evidence: {
                    let mut ev = vec![
                        format!("ERROR 日志数: {}", logs.error_count),
                        format!("WARN 日志数: {}", logs.warn_count),
                        format!("涉及服务: {}", logs.error_services.join(", ")),
                    ];
                    for cls in all_classes.iter().take(5) {
                        ev.push(format!("异常类: {}", cls));
                    }
                    ev
                },
                short_term,
                mid_term: vec![
                    "增加异常监控告警".into(),
                    "补充异常场景的单元测试".into(),
                ],
                long_term: vec![
                    "建立异常分类知识库".into(),
                    "接入统一异常追踪平台".into(),
                ],
            });
        } else {
            // 有 ERROR 但无异常类（可能是非 Exception 错误）
            findings.push(Finding {
                finding_type: FindingType::BackendException,
                severity: Severity::Medium,
                summary: format!("服务存在 {} 条 ERROR 日志", logs.error_count),
                evidence: vec![
                    format!("ERROR 日志数: {}", logs.error_count),
                    format!("涉及服务: {}", logs.error_services.join(", ")),
                ],
                short_term: vec!["检查 ERROR 日志详情定位原因".into()],
                mid_term: vec!["增加异常监控告警".into()],
                long_term: vec!["建立异常分类知识库".into()],
            });
        }
    }

    // ─── 规则 3: HTTP 错误 ───
    let error_reqs: Vec<&RequestSummary> = requests.iter()
        .filter(|r| r.status >= 500)
        .collect();

    if !error_reqs.is_empty() {
        // 按接口去重，避免同一接口多次出现
        let mut seen = std::collections::HashSet::new();
        for req in error_reqs {
            if seen.insert(&req.api_path) {
                findings.push(Finding {
                    finding_type: FindingType::HttpError,
                    severity: Severity::High,
                    summary: format!(
                        "接口 {} 返回服务端错误 HTTP {}",
                        req.api_path, req.status
                    ),
                    evidence: vec![
                        format!("URL: {}", req.url),
                        format!("服务: {}", req.service),
                        format!("状态码: {}", req.status),
                        format!("耗时: {}ms", req.duration_ms),
                    ],
                    short_term: vec!["检查服务端日志定位异常原因".into()],
                    mid_term: vec!["增加接口异常兜底和降级".into()],
                    long_term: vec!["建立接口健康巡检".into()],
                });
            }
        }
    }

    // ─── 规则 4: 慢接口但该 traceId 无 SQL（下游或计算问题）───
    let sql_trace_ids: std::collections::HashSet<&str> = sql_traces.iter()
        .map(|s| s.trace_id.as_str())
        .collect();

    for req in requests {
        if req.duration_ms <= 2000 { continue; }

        let has_sql = req.trace_id.as_deref()
            .map(|tid| sql_trace_ids.contains(tid))
            .unwrap_or(false);

        if !has_sql {
            findings.push(Finding {
                finding_type: FindingType::SlowApi,
                severity: Severity::Medium,
                summary: format!(
                    "接口 {} 响应慢 ({}ms)，未在日志中发现关联 SQL",
                    req.api_path, req.duration_ms
                ),
                evidence: vec![
                    format!("接口耗时: {}ms", req.duration_ms),
                    format!("traceId: {}", req.trace_id.as_deref().unwrap_or("缺失")),
                    "可能原因：下游服务调用慢、Redis 阻塞、MQ 超时、计算密集".into(),
                ],
                short_term: vec![
                    "检查服务间调用链路和下游依赖".into(),
                    "查看该 traceId 的完整日志".into(),
                ],
                mid_term: vec!["接入分布式 Trace（SkyWalking / OpenTelemetry）".into()],
                long_term: vec!["建立全链路性能基线".into()],
            });
        }
    }

    // ─── 规则 5: 缺失 TraceId ───
    let missing_trace: Vec<&RequestSummary> = requests.iter()
        .filter(|r| r.trace_id.is_none())
        .collect();

    if !missing_trace.is_empty() {
        findings.push(Finding {
            finding_type: FindingType::MissingTrace,
            severity: Severity::Medium,
            summary: format!(
                "{}/{} 个请求缺失 traceId，无法关联后端日志",
                missing_trace.len(), requests.len()
            ),
            evidence: {
                let mut ev = vec![format!(
                    "总请求数: {}，缺失 traceId: {}",
                    requests.len(), missing_trace.len()
                )];
                for req in missing_trace.iter().take(3) {
                    ev.push(format!("缺失示例: {} {}", req.method, req.api_path));
                }
                ev
            },
            short_term: vec!["检查网关是否正确透传 x-trace header".into()],
            mid_term: vec!["网关层面强制生成 traceId".into()],
            long_term: vec!["全链路 Trace 标准化".into()],
        });
    }

    // 按 severity 排序
    findings.sort_by_key(|f| severity_order(&f.severity));
    findings
}

fn severity_order(s: &Severity) -> u8 {
    match s {
        Severity::Critical => 0,
        Severity::High => 1,
        Severity::Medium => 2,
        Severity::Low => 3,
        Severity::Info => 4,
    }
}

fn classify_exception_list(classes: &[String]) -> (Severity, Vec<String>) {
    let has_oom = classes.iter().any(|c| c.contains("OutOfMemory"));
    let has_timeout = classes.iter().any(|c| c.contains("Timeout") || c.contains("SQLTimeout"));
    let has_npe = classes.iter().any(|c| c.contains("NullPointer"));

    if has_oom {
        (Severity::Critical, vec![
            "立即检查 JVM 堆内存配置".into(),
            "排查内存泄漏并重启服务".into(),
            "增加 GC 监控".into(),
        ])
    } else if has_timeout {
        (Severity::High, vec![
            "检查数据库连接池和超时配置".into(),
            "排查是否有长事务锁表".into(),
        ])
    } else if has_npe {
        (Severity::High, vec![
            "定位空指针代码位置，增加参数校验".into(),
            "检查可能返回 null 的外部调用".into(),
        ])
    } else {
        (Severity::High, vec![
            "排查各异常类的触发原因".into(),
            "检查相关服务日志".into(),
        ])
    }
}
