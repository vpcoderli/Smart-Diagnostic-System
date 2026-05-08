use crate::models::{DiagnosisManifest, DiagnosisPackage, MaskingReport};
use anyhow::Result;
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

    // manifest.json
    zip.start_file("manifest.json", options)?;
    zip.write_all(serde_json::to_string_pretty(&package.manifest)?.as_bytes())?;

    // browser/page.json
    zip.start_file("browser/page.json", options)?;
    let page_info = serde_json::json!({ "pageUrl": package.captured_page.page_url });
    zip.write_all(serde_json::to_string_pretty(&page_info)?.as_bytes())?;

    // browser/requests.json
    zip.start_file("browser/requests.json", options)?;
    zip.write_all(serde_json::to_string_pretty(&package.captured_page.requests)?.as_bytes())?;

    // services/{service}/app-log.jsonl
    let mut services_written = std::collections::HashSet::new();
    for log in &package.logs {
        let svc = &log.service;
        let file_path = format!("services/{}/app-log.jsonl", svc);
        if !services_written.contains(svc) {
            services_written.insert(svc.clone());
        }
        // 为简化，每条日志追加写入（实际实现中可按服务分组后一次写入）
        zip.start_file(&file_path, options)?;
        for l in package.logs.iter().filter(|l| l.service == *svc) {
            zip.write_all(serde_json::to_string(l)?.as_bytes())?;
            zip.write_all(b"\n")?;
        }
    }

    // database/slow-sql.json
    if !package.slow_sqls.is_empty() {
        zip.start_file("database/slow-sql.json", options)?;
        zip.write_all(serde_json::to_string_pretty(&package.slow_sqls)?.as_bytes())?;
    }

    // database/table-stats.json
    if !package.table_stats.is_empty() {
        zip.start_file("database/table-stats.json", options)?;
        zip.write_all(serde_json::to_string_pretty(&package.table_stats)?.as_bytes())?;
    }

    // privacy/masking-report.json
    zip.start_file("privacy/masking-report.json", options)?;
    zip.write_all(serde_json::to_string_pretty(masking_report)?.as_bytes())?;

    zip.finish()?;
    Ok(())
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
        crate::models::CapturedPage {
            page_url,
            requests,
        }
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

    Ok(DiagnosisPackage {
        manifest,
        captured_page,
        logs,
        slow_sqls,
        table_stats,
    })
}
