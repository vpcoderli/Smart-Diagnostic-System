use diag_core::models::{DiagnosisPackage, Finding};

use crate::commands::{LogSummary, RequestSummary, SqlSummary};

/// 生成 Markdown 诊断报告
pub fn generate_markdown(
    package: &DiagnosisPackage,
    requests: &[RequestSummary],
    logs: &LogSummary,
    sqls: &[SqlSummary],
    findings: &[Finding],
) -> String {
    let m = &package.manifest;
    let mut md = String::with_capacity(4096);

    // 标题
    md.push_str("# 线上问题诊断报告\n\n");

    // 基本信息
    md.push_str("## 1. 基本信息\n\n");
    md.push_str(&format!("| 项目 | 值 |\n|---|---|\n"));
    md.push_str(&format!("| 诊断 ID | `{}` |\n", m.diagnosis_id));
    md.push_str(&format!("| 医院站点 | {} |\n", m.site));
    md.push_str(&format!("| 系统 | {} |\n", m.system));
    md.push_str(&format!("| 诊断时间 | {} |\n", m.created_at));
    md.push_str(&format!("| 页面 URL | {} |\n", m.page_url));
    md.push_str(&format!("| 涉及服务 | {} |\n", m.services.join(", ")));
    md.push_str(&format!("| 数据库类型 | {} |\n", m.database_type));
    md.push_str("\n");

    // 请求概览
    md.push_str("## 2. 请求概览\n\n");
    md.push_str("| 接口路径 | 服务 | 方法 | 耗时 | 状态码 | TraceId | 风险 |\n");
    md.push_str("|---------|------|------|-----:|-------:|---------|------|\n");
    for req in requests {
        let trace_display = req
            .trace_id
            .as_deref()
            .map(|t| {
                if t.len() > 8 {
                    format!("{}...", &t[..8])
                } else {
                    t.to_string()
                }
            })
            .unwrap_or_else(|| "❌ 缺失".to_string());

        let risk_icon = match req.risk_level.as_str() {
            "ERROR" => "🔴",
            "SLOW" => "🟡",
            "WARN" => "🟠",
            _ => "🟢",
        };

        md.push_str(&format!(
            "| {} | {} | {} | {}ms | {} | {} | {} |\n",
            req.api_path, req.service, req.method, req.duration_ms, req.status, trace_display, risk_icon
        ));
    }
    md.push_str("\n");

    // 日志分析
    md.push_str("## 3. 日志分析\n\n");
    md.push_str(&format!("- 总日志行数: **{}**\n", logs.total_lines));
    md.push_str(&format!("- ERROR 数量: **{}**\n", logs.error_count));
    md.push_str(&format!("- WARN 数量: **{}**\n", logs.warn_count));
    if !logs.exception_classes.is_empty() {
        md.push_str(&format!(
            "- 异常类型: {}\n",
            logs.exception_classes
                .iter()
                .map(|e| format!("`{}`", e))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !logs.error_services.is_empty() {
        md.push_str(&format!(
            "- 异常服务: {}\n",
            logs.error_services.join(", ")
        ));
    }
    md.push_str("\n");

    // SQL 分析
    if !sqls.is_empty() {
        md.push_str("## 4. SQL 分析\n\n");
        md.push_str("| SQL 指纹 | 耗时 | 表 | 扫描行数 | 返回行数 | 风险 |\n");
        md.push_str("|---------|-----:|---|--------:|--------:|------|\n");
        for sql in sqls {
            let fp_display = if sql.sql_fingerprint.len() > 60 {
                format!("{}...", &sql.sql_fingerprint[..60])
            } else {
                sql.sql_fingerprint.clone()
            };
            let risk_icon = match sql.risk_level.as_str() {
                "HIGH" => "🔴",
                "MEDIUM" => "🟡",
                _ => "🟢",
            };
            md.push_str(&format!(
                "| `{}` | {:.0}ms | {} | {} | {} | {} |\n",
                fp_display,
                sql.duration_ms,
                sql.tables.join(", "),
                sql.rows_examined.map(|v| v.to_string()).unwrap_or("-".into()),
                sql.rows_returned.map(|v| v.to_string()).unwrap_or("-".into()),
                risk_icon
            ));
        }

        // SQL 风险详情
        for sql in sqls.iter().filter(|s| !s.risk_reasons.is_empty()) {
            md.push_str(&format!(
                "\n**`{}`** 风险详情:\n",
                if sql.tables.is_empty() {
                    "unknown".to_string()
                } else {
                    sql.tables.join(", ")
                }
            ));
            for reason in &sql.risk_reasons {
                md.push_str(&format!("- ⚠️ {}\n", reason));
            }
        }
        md.push_str("\n");
    }

    // 诊断结论
    if !findings.is_empty() {
        md.push_str("## 5. 诊断结论\n\n");
        for (i, finding) in findings.iter().enumerate() {
            let severity_icon = match finding.severity {
                diag_core::models::Severity::Critical => "🔴 严重",
                diag_core::models::Severity::High => "🟠 高",
                diag_core::models::Severity::Medium => "🟡 中",
                diag_core::models::Severity::Low => "🟢 低",
                diag_core::models::Severity::Info => "ℹ️ 信息",
            };

            md.push_str(&format!("### 结论 {}: {} [{}]\n\n", i + 1, finding.summary, severity_icon));
            md.push_str("**证据:**\n");
            for ev in &finding.evidence {
                md.push_str(&format!("- {}\n", ev));
            }
            md.push_str("\n");
        }
    }

    // 建议
    if !findings.is_empty() {
        md.push_str("## 6. 解决方案\n\n");

        md.push_str("### 短期（立即可执行）\n\n");
        let mut idx = 1;
        for f in findings {
            for s in &f.short_term {
                md.push_str(&format!("{}. {}\n", idx, s));
                idx += 1;
            }
        }

        md.push_str("\n### 中期（1-2 周）\n\n");
        idx = 1;
        for f in findings {
            for s in &f.mid_term {
                md.push_str(&format!("{}. {}\n", idx, s));
                idx += 1;
            }
        }

        md.push_str("\n### 长期（1-3 月）\n\n");
        idx = 1;
        for f in findings {
            for s in &f.long_term {
                md.push_str(&format!("{}. {}\n", idx, s));
                idx += 1;
            }
        }
        md.push_str("\n");
    }

    // 附录
    md.push_str("---\n\n");
    md.push_str(&format!(
        "*报告生成时间: {} | 收集端版本: {} | 隐私级别: {}*\n",
        m.created_at, m.collector_version, m.privacy_level
    ));

    md
}
