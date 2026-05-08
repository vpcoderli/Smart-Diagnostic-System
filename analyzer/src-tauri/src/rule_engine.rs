use diag_core::models::{DiagnosisPackage, Finding, FindingType, Severity};

use crate::commands::{LogSummary, RequestSummary, SqlSummary};

/// 基于证据的规则引擎，输出根因候选
pub fn diagnose(
    _package: &DiagnosisPackage,
    requests: &[RequestSummary],
    logs: &LogSummary,
    sqls: &[SqlSummary],
) -> Vec<Finding> {
    let mut findings = Vec::new();

    // ─── 规则 1: 慢 SQL 主导 ───
    for req in requests {
        if req.duration_ms > 1000 {
            // 查找该请求时间范围内的慢 SQL
            for sql in sqls {
                let ratio = sql.duration_ms / req.duration_ms as f64;
                if ratio > 0.5 && sql.duration_ms > 800.0 {
                    findings.push(Finding {
                        finding_type: FindingType::SlowSql,
                        severity: Severity::High,
                        summary: format!(
                            "接口 {} ({}) 主要耗时集中在数据库查询，SQL 占总耗时 {:.0}%",
                            req.api_path, req.service, ratio * 100.0
                        ),
                        evidence: vec![
                            format!("接口耗时: {}ms", req.duration_ms),
                            format!("SQL 耗时: {:.0}ms", sql.duration_ms),
                            format!("涉及表: {}", sql.tables.join(", ")),
                            format!("风险原因: {}", sql.risk_reasons.join("; ")),
                        ],
                        short_term: vec![
                            "限制空条件查询，增加默认过滤条件".into(),
                            "限制 pageSize 上限（建议 ≤ 50）".into(),
                            "对高频查询字段增加临时索引".into(),
                        ],
                        mid_term: vec![
                            "重构动态查询条件，避免全表扫描".into(),
                            "增加组合索引覆盖常用查询".into(),
                            "补充大数据量 explain 分析".into(),
                        ],
                        long_term: vec![
                            "建立数据归档机制".into(),
                            "建立接口 SLO 和慢接口巡检".into(),
                            "建立按医院数据量的性能基线".into(),
                        ],
                    });
                }
            }
        }
    }

    // ─── 规则 2: 后端异常 ───
    if logs.error_count > 0 {
        for exc_class in &logs.exception_classes {
            let (severity, short_term) = classify_exception(exc_class);

            findings.push(Finding {
                finding_type: FindingType::BackendException,
                severity,
                summary: format!(
                    "服务 {} 抛出异常: {}（共 {} 条 ERROR）",
                    logs.error_services.join(", "),
                    exc_class,
                    logs.error_count
                ),
                evidence: vec![
                    format!("异常类: {}", exc_class),
                    format!("ERROR 日志数: {}", logs.error_count),
                    format!("WARN 日志数: {}", logs.warn_count),
                    format!("涉及服务: {}", logs.error_services.join(", ")),
                ],
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
        }
    }

    // ─── 规则 3: HTTP 错误 ───
    for req in requests {
        if req.status >= 500 {
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
                ],
                short_term: vec!["检查服务端日志定位异常原因".into()],
                mid_term: vec!["增加接口异常兜底和降级".into()],
                long_term: vec!["建立接口健康巡检".into()],
            });
        }
    }

    // ─── 规则 4: 慢接口但无慢 SQL ───
    for req in requests {
        if req.duration_ms > 2000 && sqls.is_empty() {
            findings.push(Finding {
                finding_type: FindingType::SlowApi,
                severity: Severity::Medium,
                summary: format!(
                    "接口 {} 响应慢 ({}ms) 但未检测到慢 SQL",
                    req.api_path, req.duration_ms
                ),
                evidence: vec![
                    format!("接口耗时: {}ms", req.duration_ms),
                    "可能原因: 下游服务调用慢、Redis 阻塞、MQ 超时、计算密集".into(),
                ],
                short_term: vec!["检查服务间调用链路和下游依赖".into()],
                mid_term: vec!["接入分布式 Trace（SkyWalking / OpenTelemetry）".into()],
                long_term: vec!["建立全链路性能基线".into()],
            });
        }
    }

    // ─── 规则 5: 缺失 TraceId ───
    let missing_trace_count = requests.iter().filter(|r| r.trace_id.is_none()).count();
    if missing_trace_count > 0 {
        findings.push(Finding {
            finding_type: FindingType::MissingTrace,
            severity: Severity::Medium,
            summary: format!(
                "有 {} 个请求缺失 traceId，无法关联后端日志",
                missing_trace_count
            ),
            evidence: vec![format!(
                "总请求数: {}，缺失 traceId: {}",
                requests.len(),
                missing_trace_count
            )],
            short_term: vec!["检查网关是否正确透传 x-trace header".into()],
            mid_term: vec!["网关层面强制生成 traceId".into()],
            long_term: vec!["全链路 Trace 标准化".into()],
        });
    }

    // 按 severity 排序
    findings.sort_by(|a, b| severity_order(&a.severity).cmp(&severity_order(&b.severity)));
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

fn classify_exception(exc_class: &str) -> (Severity, Vec<String>) {
    if exc_class.contains("Timeout") || exc_class.contains("SQLTimeout") {
        (
            Severity::High,
            vec![
                "检查数据库连接池和超时配置".into(),
                "排查是否有长事务锁表".into(),
            ],
        )
    } else if exc_class.contains("NullPointer") {
        (
            Severity::High,
            vec![
                "定位空指针异常代码位置".into(),
                "增加参数校验和空值保护".into(),
            ],
        )
    } else if exc_class.contains("OutOfMemory") {
        (
            Severity::Critical,
            vec![
                "检查 JVM 堆内存配置".into(),
                "排查是否有内存泄漏".into(),
                "增加 GC 监控".into(),
            ],
        )
    } else {
        (
            Severity::High,
            vec![
                format!("排查 {} 的触发原因", exc_class),
                "检查相关服务日志".into(),
            ],
        )
    }
}
