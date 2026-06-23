//! diag-core 集成测试
//! 覆盖完整数据流：构建 mock 诊断包 → 打包 → 解包 → 验证数据完整性

use diag_core::config::{
    CollectorConfig, CollectorSettings, DatabaseConfig, ElkConfig, FieldMapping, GatewayConfig,
    NacosConfig, PrivacyConfig, ScheduleConfig, ServiceConfig, SiteConfig, SshConfig,
};
use diag_core::models::*;
use diag_core::package::{
    build_package, build_quick_package_with_manifest, build_realtime_package,
    build_realtime_package_with_manifest, read_package,
};
use std::io::Write;
use tempfile::tempdir;

// ─── Mock 数据工厂 ───

fn mock_collector_config() -> CollectorConfig {
    CollectorConfig {
        site: SiteConfig {
            name: "test-hospital".into(),
            system: "pcm".into(),
        },
        gateway: GatewayConfig {
            prefix: "/gateway".into(),
        },
        services: vec![ServiceConfig {
            name: "pcm-management".into(),
            display: "业务管理服务".into(),
            hosts: vec!["10.0.0.1".into()],
            log_dir: "/opt/logs/".into(),
            log_pattern: "*.log".into(),
        }],
        ssh: SshConfig {
            port: 22,
            username: "ops".into(),
            auth_type: "password".into(),
            private_key: None,
            password: Some("pass".into()),
        },
        database: DatabaseConfig {
            db_type: "mysql".into(),
            host: "10.0.0.100".into(),
            port: 3306,
            username: "readonly".into(),
            password: "dbpass".into(),
            database: "pcm_db".into(),
            schemas: Vec::new(),
        },
        privacy: PrivacyConfig {
            mask_query_values: true,
            allowed_query_keys: vec!["pageNum".into(), "pageSize".into()],
        },
        collector: CollectorSettings {
            time_window_minutes: 30,
            max_log_lines: 500,
            output_dir: "/tmp/diag-output".into(),
        },
        elk: Some(ElkConfig {
            address: "http://elk:9200".into(),
            index_pattern: "logstash-*".into(),
            username: Some("admin".into()),
            password: Some("pass".into()),
            timeout_secs: 30,
            max_hits_per_trace: 1000,
            field_mapping: FieldMapping::default(),
        }),
        nacos: None,
        schedule: Some(ScheduleConfig {
            enabled: true,
            interval_minutes: 5,
            lookback_minutes: 6,
            overlap_minutes: 1,
            levels: vec!["ERROR".into(), "WARN".into()],
            extra_keywords: vec![],
            service_filter: None,
            max_trace_ids_per_run: 50,
            dedup_window_minutes: 60,
            output_retention_days: 7,
        }),
    }
}

fn mock_diagnosis_package() -> (DiagnosisPackage, MaskingReport) {
    let manifest = DiagnosisManifest {
        diagnosis_id: "diag-20260603-120000".into(),
        site: "test-hospital".into(),
        system: "pcm".into(),
        created_at: "2026-06-03T12:00:00+08:00".into(),
        page_url: "http://10.0.0.1/pcm-manage/patient-list".into(),
        request_count: 3,
        services: vec!["pcm-management".into(), "pcm-user".into()],
        trace_ids: vec!["trace-abc123".into(), "trace-def456".into()],
        database_type: "mysql".into(),
        privacy_level: "MASKED".into(),
        collector_version: "0.2.0".into(),
        collection_mode: Some("realtime".into()),
        log_source: Some("elk".into()),
        gateway_prefix: Some("/gateway".into()),
        keywords: None,
        time_range: None,
    };

    let captured_page = CapturedPage {
        page_url: "http://10.0.0.1/pcm-manage/patient-list".into(),
        requests: vec![
            CapturedRequest {
                method: "GET".into(),
                url: "http://10.0.0.1/gateway/pcm-management/v1/patient/list?pageNum=1&pageSize=20&name=***".into(),
                status: 200,
                duration_ms: 3500,
                trace_id: Some("trace-abc123".into()),
                timestamp: "2026-06-03T12:00:01.000Z".into(),
                request_type: "fetch".into(),
                response_size: Some(4096),
            },
            CapturedRequest {
                method: "GET".into(),
                url: "http://10.0.0.1/gateway/pcm-user/v1/user/info?userId=***".into(),
                status: 200,
                duration_ms: 150,
                trace_id: Some("trace-def456".into()),
                timestamp: "2026-06-03T12:00:02.000Z".into(),
                request_type: "fetch".into(),
                response_size: Some(512),
            },
            CapturedRequest {
                method: "POST".into(),
                url: "http://10.0.0.1/gateway/pcm-management/v1/patient/update".into(),
                status: 500,
                duration_ms: 200,
                trace_id: None,
                timestamp: "2026-06-03T12:00:03.000Z".into(),
                request_type: "xhr".into(),
                response_size: None,
            },
        ],
    };

    let logs = vec![
        LogEntry {
            time: Some("2026-06-03T12:00:01.123+08:00".into()),
            level: "ERROR".into(),
            service: "pcm-management".into(),
            trace_id: Some("trace-abc123".into()),
            thread: Some("http-nio-8080-exec-1".into()),
            class: Some("com.pcm.service.PatientService".into()),
            method: Some("listPatients".into()),
            message: "Query timeout after 3000ms".into(),
            exception: Some("java.sql.SQLTimeoutException".into()),
            stack_trace: Some("at com.pcm.dao.PatientDao.list(PatientDao.java:42)".into()),
            raw: r#"{"level":"ERROR","traceId":"trace-abc123","message":"Query timeout"}"#.into(),
        },
        LogEntry {
            time: Some("2026-06-03T12:00:01.200+08:00".into()),
            level: "DEBUG".into(),
            service: "pcm-management".into(),
            trace_id: Some("trace-abc123".into()),
            thread: None,
            class: None,
            method: None,
            message: "==>  Preparing: SELECT id, name, org_id FROM patient WHERE org_id = ? AND status = ? ORDER BY create_time DESC".into(),
            exception: None,
            stack_trace: None,
            raw: "DEBUG ... ==>  Preparing: SELECT ...".into(),
        },
        LogEntry {
            time: Some("2026-06-03T12:00:02.000+08:00".into()),
            level: "INFO".into(),
            service: "pcm-user".into(),
            trace_id: Some("trace-def456".into()),
            thread: None,
            class: None,
            method: None,
            message: "用户信息查询成功 userId=12345".into(),
            exception: None,
            stack_trace: None,
            raw: "INFO ... 用户信息查询成功".into(),
        },
    ];

    let slow_sqls = vec![SlowSqlItem {
        trace_id: None,
        database_type: "mysql".into(),
        service: Some("pcm-management".into()),
        sql_fingerprint:
            "SELECT id, name, org_id FROM patient WHERE org_id = ? AND status = ? ORDER BY create_time DESC"
                .into(),
        duration_ms: 2800.0,
        tables: vec!["patient".into()],
        operation: Some("SELECT".into()),
        rows_examined: Some(150000),
        rows_returned: Some(50),
        index_used: Some(false),
        explain_summary: None,
    }];

    let sql_traces = vec![SqlTrace {
        trace_id: "trace-abc123".into(),
        service: "pcm-management".into(),
        sql: "SELECT id, name, org_id FROM patient WHERE org_id = ? AND status = ? ORDER BY create_time DESC".into(),
        sql_fingerprint:
            "SELECT id, name, org_id FROM patient WHERE org_id = ? AND status = ? ORDER BY create_time DESC"
                .into(),
        duration_ms: None,
        tables: vec!["patient".into()],
        timestamp: Some("2026-06-03T12:00:01.200+08:00".into()),
        parameters: None,
    }];

    let collection_report = CollectionReport {
        collected_at: "2026-06-03T12:00:05+08:00".into(),
        log_source: "elk".into(),
        log_count: 3,
        sql_trace_count: 1,
        explain_plan_count: 0,
        skipped_services: vec![],
        errors: vec![],
    };

    let package = DiagnosisPackage {
        manifest,
        captured_page,
        logs,
        slow_sqls,
        table_stats: vec![TableStats {
            schema: "pcm_db".into(),
            table_name: "patient".into(),
            row_count: 180000,
            data_size_bytes: Some(524288000),
            index_size_bytes: Some(67108864),
            indexes: vec![
                IndexInfo {
                    name: "PRIMARY".into(),
                    columns: vec!["id".into()],
                    unique: true,
                },
                IndexInfo {
                    name: "idx_org_status".into(),
                    columns: vec!["org_id".into(), "status".into()],
                    unique: false,
                },
            ],
        }],
        sql_traces,
        explain_plans: vec![],
        collection_report: Some(collection_report),
    };

    let masking_report = MaskingReport {
        masked_query_params: vec!["name".into(), "userId".into()],
        removed_headers: vec!["authorization".into(), "cookie".into()],
        masked_sql_params: true,
        total_items_masked: 2,
    };

    (package, masking_report)
}

// ─── 测试：config 序列化 ───

#[test]
fn test_elk_config_field_mapping_defaults() {
    let fm = FieldMapping::default();
    assert_eq!(fm.timestamp, "@timestamp");
    assert_eq!(fm.level, "level");
    assert_eq!(fm.service, "serviceName");
    assert_eq!(fm.trace_id, "traceId");
    assert_eq!(fm.message, "message");
}

#[test]
fn test_schedule_config_defaults() {
    let sc = ScheduleConfig::default();
    assert!(!sc.enabled);
    assert_eq!(sc.interval_minutes, 5);
    assert_eq!(sc.levels, vec!["ERROR", "WARN"]);
    assert_eq!(sc.max_trace_ids_per_run, 50);
}

#[test]
fn test_collector_config_elk_present() {
    let config = mock_collector_config();
    assert!(config.elk.is_some());
    let elk = config.elk.unwrap();
    assert_eq!(elk.address, "http://elk:9200");
    assert_eq!(elk.timeout_secs, 30);
    assert_eq!(elk.field_mapping.trace_id, "traceId");
}

#[test]
fn test_collector_config_schedule_present() {
    let config = mock_collector_config();
    assert!(config.schedule.is_some());
    let sched = config.schedule.unwrap();
    assert!(sched.enabled);
    assert_eq!(sched.interval_minutes, 5);
    assert_eq!(sched.lookback_minutes, 6);
}

#[test]
fn test_collector_config_find_service() {
    let config = mock_collector_config();
    assert!(config.find_service("pcm-management").is_some());
    assert!(config.find_service("pcm-unknown").is_none());
}

// ─── 测试：package roundtrip ───

#[test]
fn test_package_build_and_read_roundtrip() {
    let dir = tempdir().expect("无法创建临时目录");
    let zip_path = dir.path().join("test-diagnosis.zip");

    let (original_pkg, masking_report) = mock_diagnosis_package();

    // 打包
    build_package(&original_pkg, &masking_report, &zip_path).expect("build_package 失败");

    assert!(zip_path.exists(), "ZIP 文件未生成");
    assert!(zip_path.metadata().unwrap().len() > 0, "ZIP 文件为空");

    // 解包
    let loaded_pkg = read_package(&zip_path).expect("read_package 失败");

    // 验证 manifest
    assert_eq!(
        loaded_pkg.manifest.diagnosis_id,
        original_pkg.manifest.diagnosis_id
    );
    assert_eq!(loaded_pkg.manifest.site, "test-hospital");
    assert_eq!(loaded_pkg.manifest.gateway_prefix, Some("/gateway".into()));
    assert_eq!(loaded_pkg.manifest.log_source, Some("elk".into()));

    // 验证请求
    assert_eq!(loaded_pkg.captured_page.requests.len(), 3);
    assert_eq!(loaded_pkg.captured_page.requests[0].duration_ms, 3500);
    assert_eq!(
        loaded_pkg.captured_page.requests[0].trace_id,
        Some("trace-abc123".into())
    );
    assert_eq!(loaded_pkg.captured_page.requests[2].status, 500);

    // 验证日志
    assert_eq!(loaded_pkg.logs.len(), 3);
    let error_log = loaded_pkg.logs.iter().find(|l| l.level == "ERROR").unwrap();
    assert_eq!(error_log.service, "pcm-management");
    assert_eq!(error_log.trace_id, Some("trace-abc123".into()));
    assert_eq!(
        error_log.exception,
        Some("java.sql.SQLTimeoutException".into())
    );

    // 验证慢 SQL
    assert_eq!(loaded_pkg.slow_sqls.len(), 1);
    assert!((loaded_pkg.slow_sqls[0].duration_ms - 2800.0).abs() < 0.01);

    // 验证表统计
    assert_eq!(loaded_pkg.table_stats.len(), 1);
    assert_eq!(loaded_pkg.table_stats[0].table_name, "patient");
    assert_eq!(loaded_pkg.table_stats[0].row_count, 180000);

    // 验证 sql_traces
    assert_eq!(loaded_pkg.sql_traces.len(), 1);
    assert_eq!(loaded_pkg.sql_traces[0].trace_id, "trace-abc123");
    assert_eq!(loaded_pkg.sql_traces[0].service, "pcm-management");

    // 验证 CollectionReport
    let report = loaded_pkg
        .collection_report
        .expect("CollectionReport 未写入");
    assert_eq!(report.log_count, 3);
    assert_eq!(report.sql_trace_count, 1);
    assert_eq!(report.log_source, "elk");
    assert!(report.errors.is_empty());
}

#[test]
fn test_package_zip_contains_correct_files() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("test.zip");
    let (pkg, masking) = mock_diagnosis_package();
    build_package(&pkg, &masking, &zip_path).unwrap();

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();

    assert!(
        names.contains(&"manifest.json".to_string()),
        "缺 manifest.json，实际有: {:?}",
        names
    );
    assert!(
        names.contains(&"browser/requests.json".to_string()),
        "缺 browser/requests.json"
    );
    assert!(
        names
            .iter()
            .any(|n| n.starts_with("services/") && n.ends_with("app-log.jsonl")),
        "缺服务日志"
    );
    assert!(
        names.contains(&"database/slow-sql.json".to_string()),
        "缺慢SQL"
    );
    assert!(
        names.contains(&"database/table-stats.json".to_string()),
        "缺表统计"
    );
    assert!(
        names.contains(&"database/sql-traces.json".to_string()),
        "缺 sql-traces"
    );
    assert!(
        names.contains(&"privacy/masking-report.json".to_string()),
        "缺脱敏报告"
    );
    assert!(
        names.contains(&"collection_report/report.json".to_string()),
        "缺采集报告"
    );
}

#[test]
fn test_package_no_duplicate_service_log_files() {
    // 验证同一服务只有一个 app-log.jsonl
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("dedup-test.zip");
    let (pkg, masking) = mock_diagnosis_package();
    build_package(&pkg, &masking, &zip_path).unwrap();

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();

    let mgmt_log_count = (0..archive.len())
        .filter(|&i| archive.by_index(i).unwrap().name() == "services/pcm-management/app-log.jsonl")
        .count();

    assert_eq!(
        mgmt_log_count, 1,
        "pcm-management/app-log.jsonl 出现了 {} 次（应为 1）",
        mgmt_log_count
    );
}

#[test]
fn test_realtime_package_groups_logs_by_captured_request_trace_id() {
    use std::io::Read;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("realtime-test.zip");
    let (mut pkg, _) = mock_diagnosis_package();

    pkg.logs.push(LogEntry {
        time: Some("2026-06-03T12:00:04.000+08:00".into()),
        level: "INFO".into(),
        service: "pcm-management".into(),
        trace_id: Some("trace-orphan".into()),
        thread: None,
        class: None,
        method: None,
        message: "未匹配浏览器请求的后台日志".into(),
        exception: None,
        stack_trace: None,
        raw: "INFO orphan".into(),
    });

    build_realtime_package(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/gateway",
        &zip_path,
    )
    .unwrap();

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut report = String::new();
    archive
        .by_name("realtime/request-logs.md")
        .expect("缺少实时请求日志分组文件")
        .read_to_string(&mut report)
        .unwrap();

    let abc_section = report
        .split("## 2. x-trace")
        .next()
        .expect("缺少第一个请求分组");
    assert!(abc_section.contains("x-trace：`trace-abc123`"));
    assert!(abc_section.contains("Query timeout after 3000ms"));
    assert!(!abc_section.contains("用户信息查询成功"));
    assert!(!report.contains("x-trace (x0)"));
    assert!(report.contains("## 未匹配浏览器请求的日志"));
    assert!(report.contains("trace-orphan"));
}

#[test]
fn test_realtime_package_can_be_read_as_structured_package() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("realtime-readable-test.zip");
    let (pkg, _) = mock_diagnosis_package();

    build_realtime_package(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/gateway",
        &zip_path,
    )
    .unwrap();

    let loaded = read_package(&zip_path).expect("realtime package should be importable");
    assert_eq!(loaded.manifest.collection_mode.as_deref(), Some("realtime"));
    assert_eq!(
        loaded.captured_page.requests.len(),
        pkg.captured_page.requests.len()
    );
    assert_eq!(loaded.logs.len(), pkg.logs.len());
    assert_eq!(loaded.sql_traces.len(), pkg.sql_traces.len());
    assert_eq!(loaded.explain_plans.len(), pkg.explain_plans.len());
    assert_eq!(loaded.table_stats.len(), pkg.table_stats.len());
}

#[test]
fn test_realtime_package_masks_request_urls_in_markdown_and_structured_json() {
    use std::io::Read;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("realtime-masked-url-test.zip");
    let (mut pkg, _) = mock_diagnosis_package();
    pkg.captured_page.requests[0].url = "http://10.0.0.1/gateway/pcm-management/v1/patient/list?pageNum=1&pageSize=20&patientName=张三&phone=13800000000".into();
    pkg.captured_page.page_url =
        "http://10.0.0.1/pcm-manage/patient-list?patientName=李四&phone=13900000000&pageNum=1"
            .into();

    build_realtime_package(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/gateway",
        &zip_path,
    )
    .unwrap();

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();

    let mut overview = String::new();
    archive
        .by_name("realtime/overview.md")
        .unwrap()
        .read_to_string(&mut overview)
        .unwrap();
    assert!(!overview.contains("李四"));
    assert!(!overview.contains("13900000000"));
    assert!(!overview.contains("张三"));
    assert!(!overview.contains("13800000000"));

    let mut request_logs = String::new();
    archive
        .by_name("realtime/request-logs.md")
        .unwrap()
        .read_to_string(&mut request_logs)
        .unwrap();
    assert!(request_logs.contains("pageNum=1"));
    assert!(request_logs.contains("pageSize=20"));
    assert!(!request_logs.contains("李四"));
    assert!(!request_logs.contains("13900000000"));
    assert!(!request_logs.contains("张三"));
    assert!(!request_logs.contains("13800000000"));

    drop(archive);
    let loaded = read_package(&zip_path).unwrap();
    let stored_url = &loaded.captured_page.requests[0].url;
    assert!(stored_url.contains("pageNum=1"));
    assert!(stored_url.contains("pageSize=20"));
    assert!(!stored_url.contains("张三"));
    assert!(!stored_url.contains("13800000000"));
    assert!(!loaded.manifest.page_url.contains("李四"));
    assert!(!loaded.manifest.page_url.contains("13900000000"));
    assert!(loaded.manifest.page_url.contains("pageNum=1"));
}

#[test]
fn test_realtime_package_masks_relative_request_urls() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("realtime-relative-masked-url-test.zip");
    let (mut pkg, _) = mock_diagnosis_package();
    pkg.captured_page.requests[0].url =
        "/gateway/pcm-management/v1/patient/list?pageNum=1&pageSize=20&patientName=张三&phone=13800000000"
            .into();

    build_realtime_package(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/gateway",
        &zip_path,
    )
    .unwrap();

    let loaded = read_package(&zip_path).unwrap();
    let stored_url = &loaded.captured_page.requests[0].url;
    assert!(stored_url.contains("pageNum=1"));
    assert!(stored_url.contains("pageSize=20"));
    assert!(!stored_url.contains("张三"));
    assert!(!stored_url.contains("13800000000"));
}

#[test]
fn test_realtime_package_with_manifest_preserves_metadata_and_collection_report() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("realtime-manifest-report-test.zip");
    let (mut pkg, _) = mock_diagnosis_package();
    let privacy = mock_collector_config().privacy;
    pkg.manifest.diagnosis_id = "diag-real-001".into();
    pkg.manifest.site = "real-hospital".into();
    pkg.manifest.system = "pcm-real".into();
    pkg.manifest.database_type = "postgresql".into();
    pkg.manifest.collector_version = "collector-9.9.9".into();
    pkg.manifest.collection_mode = Some("realtime".into());
    pkg.manifest.log_source = Some("elk".into());
    pkg.manifest.gateway_prefix = Some("/custom-gateway".into());
    pkg.manifest.page_url = "http://10.0.0.1/pcm?pageNum=1&patientName=王五".into();
    pkg.captured_page.page_url = pkg.manifest.page_url.clone();
    let report = CollectionReport {
        collected_at: "2026-06-23T11:30:00+08:00".into(),
        log_source: "elk".into(),
        log_count: 3,
        sql_trace_count: 1,
        explain_plan_count: 0,
        skipped_services: vec!["unknown（URL 无法解析服务名）".into()],
        errors: vec!["DB 采集失败: timeout".into()],
    };

    build_realtime_package_with_manifest(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/custom-gateway",
        &pkg.manifest,
        Some(&report),
        &privacy,
        &zip_path,
    )
    .unwrap();

    let loaded = read_package(&zip_path).unwrap();
    assert_eq!(loaded.manifest.diagnosis_id, "diag-real-001");
    assert_eq!(loaded.manifest.site, "real-hospital");
    assert_eq!(loaded.manifest.system, "pcm-real");
    assert_eq!(loaded.manifest.database_type, "postgresql");
    assert_eq!(loaded.manifest.collector_version, "collector-9.9.9");
    assert_eq!(loaded.manifest.collection_mode.as_deref(), Some("realtime"));
    assert_eq!(loaded.manifest.log_source.as_deref(), Some("elk"));
    assert_eq!(
        loaded.manifest.gateway_prefix.as_deref(),
        Some("/custom-gateway")
    );
    assert!(loaded.manifest.page_url.contains("pageNum=1"));
    assert!(!loaded.manifest.page_url.contains("王五"));

    let loaded_report = loaded
        .collection_report
        .expect("collection report should be preserved");
    assert_eq!(loaded_report.log_source, "elk");
    assert_eq!(
        loaded_report.errors,
        vec!["DB 采集失败: timeout".to_string()]
    );
    assert_eq!(
        loaded_report.skipped_services,
        vec!["unknown（URL 无法解析服务名）".to_string()]
    );
}

#[test]
fn test_realtime_package_with_manifest_uses_supplied_privacy_config() {
    use std::io::Read;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("realtime-custom-privacy-test.zip");
    let (mut pkg, _) = mock_diagnosis_package();
    pkg.captured_page.page_url =
        "http://10.0.0.1/page?status=OPEN&pageNum=1&patientName=赵六".into();
    pkg.manifest.page_url = pkg.captured_page.page_url.clone();
    pkg.captured_page.requests[0].url = "http://10.0.0.1/gateway/pcm-management/v1/patient/list?status=OPEN&pageNum=1&patientName=赵六".into();
    let privacy = PrivacyConfig {
        mask_query_values: true,
        allowed_query_keys: vec!["pageNum".into()],
    };

    build_realtime_package_with_manifest(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/gateway",
        &pkg.manifest,
        None,
        &privacy,
        &zip_path,
    )
    .unwrap();

    let mut archive = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
    let mut request_logs = String::new();
    archive
        .by_name("realtime/request-logs.md")
        .unwrap()
        .read_to_string(&mut request_logs)
        .unwrap();
    assert!(request_logs.contains("pageNum=1"));
    assert!(!request_logs.contains("status=OPEN"));
    assert!(!request_logs.contains("赵六"));
    drop(archive);

    let loaded = read_package(&zip_path).unwrap();
    assert!(loaded.manifest.page_url.contains("pageNum=1"));
    assert!(!loaded.manifest.page_url.contains("status=OPEN"));
    assert!(!loaded.manifest.page_url.contains("赵六"));
    assert!(loaded.captured_page.requests[0].url.contains("pageNum=1"));
    assert!(!loaded.captured_page.requests[0].url.contains("status=OPEN"));
}

#[test]
fn test_quick_package_with_manifest_preserves_historical_mode_and_report() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("quick-manifest-report-test.zip");
    let (mut pkg, _) = mock_diagnosis_package();
    pkg.manifest.diagnosis_id = "diag-historical-001".into();
    pkg.manifest.site = "history-hospital".into();
    pkg.manifest.system = "pcm-history".into();
    pkg.manifest.database_type = "mysql".into();
    pkg.manifest.collector_version = "collector-1.2.3".into();
    pkg.manifest.collection_mode = Some("historical".into());
    pkg.manifest.log_source = Some("ssh".into());
    pkg.manifest.page_url = "historical".into();
    pkg.manifest.request_count = 0;
    let report = CollectionReport {
        collected_at: "2026-06-23T11:40:00+08:00".into(),
        log_source: "ssh".into(),
        log_count: 3,
        sql_trace_count: 1,
        explain_plan_count: 0,
        skipped_services: vec![],
        errors: vec!["SSH 日志采集失败: auth failed".into()],
    };

    build_quick_package_with_manifest(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.manifest,
        Some(&report),
        &zip_path,
    )
    .unwrap();

    let loaded = read_package(&zip_path).unwrap();
    assert_eq!(loaded.manifest.diagnosis_id, "diag-historical-001");
    assert_eq!(loaded.manifest.site, "history-hospital");
    assert_eq!(loaded.manifest.system, "pcm-history");
    assert_eq!(
        loaded.manifest.collection_mode.as_deref(),
        Some("historical")
    );
    assert_eq!(loaded.manifest.log_source.as_deref(), Some("ssh"));
    assert_eq!(loaded.manifest.request_count, 0);
    assert_eq!(loaded.captured_page.page_url, "historical");
    let loaded_report = loaded
        .collection_report
        .expect("collection report should be preserved");
    assert_eq!(
        loaded_report.errors,
        vec!["SSH 日志采集失败: auth failed".to_string()]
    );
}

#[test]
fn test_realtime_package_outputs_overview_index() {
    use std::io::Read;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("realtime-overview-test.zip");
    let (pkg, _) = mock_diagnosis_package();

    build_realtime_package(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/gateway",
        &zip_path,
    )
    .unwrap();

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut overview = String::new();
    archive
        .by_name("realtime/overview.md")
        .expect("缺少实时诊断总览文件")
        .read_to_string(&mut overview)
        .unwrap();

    assert!(overview.contains("# 实时诊断报告"));
    assert!(overview.contains(
        "| # | 风险 | traceId | 接口 | 状态码 | 耗时 | 服务 | 日志信号 | SQL | EXPLAIN |"
    ));
    assert!(overview.contains("| 1 | ERROR | `trace-abc123` | /v1/patient/list | 200 | 3500ms | pcm-management | ERROR=1 WARN=0 | 1 | 0 成功 / 0 失败 |"));
    assert!(overview.contains("| 2 | OK | `trace-def456` | /v1/user/info | 200 | 150ms | pcm-user | ERROR=0 WARN=0 | 0 | 0 成功 / 0 失败 |"));
    assert!(overview.contains("| 3 | ERROR | `无 traceId` | /v1/patient/update | 500 | 200ms | pcm-management | ERROR=0 WARN=0 | 0 | 0 成功 / 0 失败 |"));
}

#[test]
fn test_realtime_overview_counts_explain_for_shared_sql_fingerprint() {
    use std::io::Read;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("realtime-shared-fingerprint-test.zip");
    let (mut pkg, _) = mock_diagnosis_package();

    pkg.sql_traces.push(SqlTrace {
        trace_id: "trace-def456".into(),
        service: "pcm-user".into(),
        sql: pkg.sql_traces[0].sql.clone(),
        sql_fingerprint: pkg.sql_traces[0].sql_fingerprint.clone(),
        duration_ms: Some(1200.0),
        tables: pkg.sql_traces[0].tables.clone(),
        timestamp: Some("2026-06-03T12:00:02.000+08:00".into()),
        parameters: Some("218713736305705076(String), ACTIVE(String)".into()),
    });
    pkg.explain_plans.push(ExplainPlan {
        sql_fingerprint: pkg.sql_traces[0].sql_fingerprint.clone(),
        avg_duration_ms: 1500.0,
        source: "log_sql_explain".into(),
        explain_rows: vec![ExplainRow {
            id: Some(1),
            select_type: Some("SIMPLE".into()),
            table: Some("patient".into()),
            access_type: Some("ref".into()),
            possible_keys: Some("idx_org_status".into()),
            key: Some("idx_org_status".into()),
            rows: Some(10),
            filtered: Some(100.0),
            extra: Some("Using where".into()),
        }],
        table_stats: None,
        trace_id: Some("trace-abc123".into()),
        executed_sql: Some("SELECT id, name, org_id FROM patient WHERE org_id = '1' AND status = 'ACTIVE' ORDER BY create_time DESC".into()),
        error: None,
        found_in_schema: Some("pcm_db".into()),
    });

    build_realtime_package(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/gateway",
        &zip_path,
    )
    .unwrap();

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut overview = String::new();
    archive
        .by_name("realtime/overview.md")
        .unwrap()
        .read_to_string(&mut overview)
        .unwrap();

    assert!(overview.contains("| 2 | SLOW | `trace-def456` | /v1/user/info | 200 | 150ms | pcm-user | ERROR=0 WARN=0 | 1 | 1 成功 / 0 失败 |"));
}

#[test]
fn test_quick_sql_report_uses_trace_specific_executed_sql_for_shared_fingerprint() {
    use diag_core::package::build_quick_package;
    use std::io::Read;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("shared-fingerprint-sql-report.zip");
    let fingerprint = "select * from patient where id = ?".to_string();
    let traces = vec![
        SqlTrace {
            trace_id: "trace-a".into(),
            service: "pcm-management".into(),
            sql: "select * from patient where id = ?".into(),
            sql_fingerprint: fingerprint.clone(),
            duration_ms: None,
            tables: vec!["patient".into()],
            timestamp: Some("2026-06-03T12:00:00+08:00".into()),
            parameters: Some("1(Integer)".into()),
        },
        SqlTrace {
            trace_id: "trace-b".into(),
            service: "pcm-management".into(),
            sql: "select * from patient where id = ?".into(),
            sql_fingerprint: fingerprint.clone(),
            duration_ms: None,
            tables: vec!["patient".into()],
            timestamp: Some("2026-06-03T12:00:01+08:00".into()),
            parameters: Some("2(Integer)".into()),
        },
    ];
    let plans = vec![ExplainPlan {
        sql_fingerprint: fingerprint,
        avg_duration_ms: 1.0,
        source: "log_sql_explain".into(),
        explain_rows: vec![],
        table_stats: None,
        trace_id: Some("trace-a".into()),
        executed_sql: Some("select * from patient where id = 1".into()),
        error: Some("fake explain failure".into()),
        found_in_schema: None,
    }];

    build_quick_package(&[], &traces, &plans, &[], &zip_path).unwrap();

    let mut archive = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
    let mut report = String::new();
    archive
        .by_name("pcm-management_sql.md")
        .unwrap()
        .read_to_string(&mut report)
        .unwrap();

    let trace_b_section = report
        .split("| traceId | `trace-b` |")
        .nth(1)
        .expect("trace-b section missing");
    assert!(trace_b_section.contains("select * from patient where id = 2"));
    assert!(!trace_b_section.contains("select * from patient where id = 1"));
}

#[test]
fn test_realtime_request_cards_use_trace_specific_sql_for_shared_fingerprint() {
    use std::io::Read;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("shared-fingerprint-cards.zip");
    let (mut pkg, _) = mock_diagnosis_package();
    let fingerprint = "select * from patient where id = ?".to_string();
    pkg.sql_traces = vec![
        SqlTrace {
            trace_id: "trace-abc123".into(),
            service: "pcm-management".into(),
            sql: "select * from patient where id = ?".into(),
            sql_fingerprint: fingerprint.clone(),
            duration_ms: None,
            tables: vec!["patient".into()],
            timestamp: Some("2026-06-03T12:00:00+08:00".into()),
            parameters: Some("1(Integer)".into()),
        },
        SqlTrace {
            trace_id: "trace-def456".into(),
            service: "pcm-user".into(),
            sql: "select * from patient where id = ?".into(),
            sql_fingerprint: fingerprint.clone(),
            duration_ms: None,
            tables: vec!["patient".into()],
            timestamp: Some("2026-06-03T12:00:01+08:00".into()),
            parameters: Some("2(Integer)".into()),
        },
    ];
    pkg.explain_plans = vec![ExplainPlan {
        sql_fingerprint: fingerprint,
        avg_duration_ms: 1.0,
        source: "log_sql_explain".into(),
        explain_rows: vec![],
        table_stats: None,
        trace_id: Some("trace-abc123".into()),
        executed_sql: Some("select * from patient where id = 1".into()),
        error: Some("fake explain failure".into()),
        found_in_schema: None,
    }];

    build_realtime_package(
        &pkg.logs,
        &pkg.sql_traces,
        &pkg.explain_plans,
        &pkg.table_stats,
        &pkg.captured_page,
        "/gateway",
        &zip_path,
    )
    .unwrap();

    let mut archive = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
    let mut cards = String::new();
    archive
        .by_name("realtime/request-cards.md")
        .unwrap()
        .read_to_string(&mut cards)
        .unwrap();

    let trace_b_card = cards.split("## 2.").nth(1).expect("second card missing");
    assert!(trace_b_card.contains("select * from patient where id = 2"));
    assert!(!trace_b_card.contains("select * from patient where id = 1"));
}

#[test]
fn test_read_package_errors_on_malformed_sql_traces_json() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("bad-sql-traces.zip");
    let (pkg, masking) = mock_diagnosis_package();
    build_package(&pkg, &masking, &zip_path).unwrap();

    let file = std::fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("manifest.json", options).unwrap();
    zip.write_all(
        serde_json::to_string_pretty(&pkg.manifest)
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    zip.start_file("browser/requests.json", options).unwrap();
    zip.write_all(
        serde_json::to_string_pretty(&pkg.captured_page.requests)
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    zip.start_file("database/sql-traces.json", options).unwrap();
    zip.write_all(b"not json").unwrap();
    zip.finish().unwrap();

    let err = read_package(&zip_path).expect_err("malformed sql-traces should fail import");
    assert!(err.to_string().contains("sql-traces") || err.to_string().contains("expected"));
}

#[test]
fn test_read_package_errors_on_malformed_collection_report_json() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("bad-report.zip");
    let (pkg, _) = mock_diagnosis_package();

    let file = std::fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("manifest.json", options).unwrap();
    zip.write_all(
        serde_json::to_string_pretty(&pkg.manifest)
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    zip.start_file("browser/requests.json", options).unwrap();
    zip.write_all(
        serde_json::to_string_pretty(&pkg.captured_page.requests)
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    zip.start_file("collection_report/report.json", options)
        .unwrap();
    zip.write_all(b"not json").unwrap();
    zip.finish().unwrap();

    let err = read_package(&zip_path).expect_err("malformed collection report should fail import");
    assert!(err.to_string().contains("collection_report") || err.to_string().contains("expected"));
}

#[test]
fn test_quick_package_renders_table_stats_with_schema_name() {
    use diag_core::package::build_quick_package;
    use std::io::Read;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("schema-stats-test.zip");

    let traces = vec![SqlTrace {
        trace_id: "trace-sql".into(),
        service: "outbound-common-manager".into(),
        sql: "SELECT count(0) FROM `tb_name_list` WHERE `TYPE` = ?".into(),
        sql_fingerprint: "SELECT count(?) FROM `tb_name_list` WHERE `TYPE` = ?".into(),
        duration_ms: None,
        tables: vec!["tb_name_list".into()],
        timestamp: Some("2026-06-23T01:57:54.923Z".into()),
        parameters: Some("1(Integer)".into()),
    }];
    let stats = vec![TableStats {
        schema: "outbound_platform".into(),
        table_name: "tb_name_list".into(),
        row_count: 12345,
        data_size_bytes: Some(2048),
        index_size_bytes: None,
        indexes: vec![IndexInfo {
            name: "idx_type".into(),
            columns: vec!["TYPE".into()],
            unique: false,
        }],
    }];

    build_quick_package(&[], &traces, &[], &stats, &zip_path).unwrap();

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut report = String::new();
    archive
        .by_name("outbound-common-manager_sql.md")
        .unwrap()
        .read_to_string(&mut report)
        .unwrap();

    assert!(report.contains("outbound_platform.tb_name_list"));
    assert!(report.contains("12,345"));
    assert!(report.contains("idx_type"));
}

// ─── 测试：URL resolver ───

#[test]
fn test_url_resolver_pcm_management() {
    use diag_core::url_resolver::resolve_url;
    let url = "http://10.0.0.1/gateway/pcm-management/v1/patient/list?pageNum=1&name=test";
    let resolved = resolve_url(url, "/gateway").unwrap();
    assert_eq!(resolved.service, "pcm-management");
    assert_eq!(resolved.api_path, "/v1/patient/list");
    assert_eq!(resolved.host, "10.0.0.1");
}

#[test]
fn test_url_resolver_unknown_service() {
    use diag_core::url_resolver::resolve_url;
    let url = "http://10.0.0.1/gateway/unknown-svc/v1/foo";
    let resolved = resolve_url(url, "/gateway").unwrap();
    assert_eq!(resolved.service, "unknown-svc");
}

#[test]
fn test_url_resolver_non_gateway_url() {
    use diag_core::url_resolver::resolve_url;
    // 非网关 URL，不会 panic
    let url = "http://10.0.0.1/static/js/app.js";
    let _ = resolve_url(url, "/gateway"); // 只要不 panic
}

// ─── 测试：masking ───

#[test]
fn test_masking_url_replaces_non_whitelist() {
    use diag_core::config::PrivacyConfig;
    use diag_core::masking::mask_url;

    let config = PrivacyConfig {
        mask_query_values: true,
        allowed_query_keys: vec!["pageNum".into(), "pageSize".into()],
    };
    let url = "http://host/gateway/pcm-management/v1/list?pageNum=1&pageSize=20&patientName=%E5%BC%A0%E4%B8%89&phone=13800000000";
    let masked = mask_url(url, &config);

    assert!(masked.contains("pageNum=1"), "pageNum 应保留原值");
    assert!(masked.contains("pageSize=20"), "pageSize 应保留原值");
    // patientName and phone should be masked
    assert!(
        !masked.contains("%E5%BC%A0%E4%B8%89") && !masked.contains("张三"),
        "患者姓名应被脱敏"
    );
    assert!(!masked.contains("13800000000"), "手机号应被脱敏");
}

#[test]
fn test_masking_disabled() {
    use diag_core::config::PrivacyConfig;
    use diag_core::masking::mask_url;

    let config = PrivacyConfig {
        mask_query_values: false,
        allowed_query_keys: vec![],
    };
    let url = "http://host/api?name=hello";
    let result = mask_url(url, &config);
    assert!(result.contains("name=hello"), "脱敏关闭时应保留原值");
}

// ─── 测试：log parser ───

#[test]
fn test_log_parser_json_format() {
    use diag_core::log_parser::parse_log_line;
    let line = r#"{"time":"2026-06-03T12:00:01+08:00","level":"ERROR","service":"pcm-management","traceId":"trace-abc","message":"Query timeout","exception":"java.sql.SQLTimeoutException"}"#;
    let entry = parse_log_line(line, "pcm-management");
    assert_eq!(entry.level, "ERROR");
    assert_eq!(entry.trace_id, Some("trace-abc".into()));
    assert_eq!(entry.exception, Some("java.sql.SQLTimeoutException".into()));
    assert_eq!(entry.service, "pcm-management");
}

#[test]
fn test_log_parser_text_format() {
    use diag_core::log_parser::parse_log_line;
    let line = "2026-06-03 12:00:01.123 ERROR [http-nio-8080-exec-1] c.p.PatientService - NullPointerException: org is null";
    let entry = parse_log_line(line, "pcm-server");
    assert_eq!(entry.service, "pcm-server");
    // level 应被识别
    assert!(!entry.level.is_empty());
    assert_eq!(entry.level, "ERROR");
}

#[test]
fn test_log_parser_extract_trace_id() {
    use diag_core::log_parser::extract_trace_id;
    let json_line = r#"{"traceId":"abc123","level":"INFO"}"#;
    assert_eq!(extract_trace_id(json_line), Some("abc123".into()));

    let mdc_line = "2026-06-03 INFO [traceId=def456] Service - msg";
    assert_eq!(extract_trace_id(mdc_line), Some("def456".into()));

    let no_trace = "2026-06-03 INFO Service - no trace here";
    assert_eq!(extract_trace_id(no_trace), None);
}

// ─── 测试：sql parser ───

#[test]
fn test_sql_fingerprint() {
    use diag_core::sql_parser::fingerprint_sql;
    let sql = "SELECT id, name FROM patient WHERE org_id = 123 AND status = 'ACTIVE' LIMIT 50";
    let fp = fingerprint_sql(sql);
    assert!(!fp.contains("123"), "数字应被替换为?");
    assert!(!fp.contains("ACTIVE"), "字符串应被替换为?");
    assert!(fp.contains("?"), "应有?占位符");
}

#[test]
fn test_sql_extract_tables() {
    use diag_core::sql_parser::extract_tables;
    let sql = "SELECT a.id FROM patient a JOIN org b ON a.org_id = b.id WHERE b.status = 'ACTIVE'";
    let tables = extract_tables(sql);
    assert!(
        tables.contains(&"patient".into()),
        "应提取 patient 表，实际: {:?}",
        tables
    );
    assert!(
        tables.contains(&"org".into()),
        "应提取 org 表，实际: {:?}",
        tables
    );
}

#[test]
fn test_sql_detect_operation() {
    use diag_core::sql_parser::detect_operation;
    assert_eq!(detect_operation("SELECT * FROM t"), "SELECT");
    assert_eq!(detect_operation("  UPDATE t SET a=1"), "UPDATE");
    assert_eq!(detect_operation("INSERT INTO t VALUES(1)"), "INSERT");
    assert_eq!(detect_operation("DELETE FROM t WHERE id=1"), "DELETE");
    assert_eq!(detect_operation("CALL proc()"), "OTHER");
}

// ─── 测试：models 序列化 ───

#[test]
fn test_models_serde_roundtrip() {
    let (pkg, _) = mock_diagnosis_package();
    let json = serde_json::to_string(&pkg).expect("序列化失败");
    let deserialized: DiagnosisPackage = serde_json::from_str(&json).expect("反序列化失败");
    assert_eq!(
        deserialized.manifest.diagnosis_id,
        pkg.manifest.diagnosis_id
    );
    assert_eq!(deserialized.sql_traces.len(), 1);
    assert_eq!(deserialized.logs.len(), 3);
}

#[test]
fn test_time_window_serde() {
    let tw = TimeWindow {
        start: "2026-06-03T12:00:00+08:00".into(),
        end: "2026-06-03T12:30:00+08:00".into(),
    };
    let json = serde_json::to_string(&tw).unwrap();
    let tw2: TimeWindow = serde_json::from_str(&json).unwrap();
    assert_eq!(tw2.start, tw.start);
}

#[test]
fn test_collection_report_default() {
    let report = CollectionReport::default();
    assert_eq!(report.log_count, 0);
    assert!(report.errors.is_empty());
    assert!(report.skipped_services.is_empty());
}

// ─── 测试：package roundtrip（空 slow_sqls / sql_traces）───

#[test]
fn test_package_roundtrip_empty_optionals() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("empty-optionals.zip");

    let manifest = DiagnosisManifest {
        diagnosis_id: "diag-empty".into(),
        site: "hosp".into(),
        system: "pcm".into(),
        created_at: "2026-06-03T00:00:00+08:00".into(),
        page_url: "http://h/page".into(),
        request_count: 0,
        services: vec![],
        trace_ids: vec![],
        database_type: "mysql".into(),
        privacy_level: "MASKED".into(),
        collector_version: "0.2.0".into(),
        collection_mode: None,
        log_source: None,
        gateway_prefix: None,
        keywords: None,
        time_range: None,
    };
    let pkg = DiagnosisPackage {
        manifest,
        captured_page: CapturedPage {
            page_url: "http://h/page".into(),
            requests: vec![],
        },
        logs: vec![],
        slow_sqls: vec![],
        table_stats: vec![],
        sql_traces: vec![],
        explain_plans: vec![],
        collection_report: None,
    };
    let masking = MaskingReport {
        masked_query_params: vec![],
        removed_headers: vec![],
        masked_sql_params: false,
        total_items_masked: 0,
    };

    build_package(&pkg, &masking, &zip_path).unwrap();
    let loaded = read_package(&zip_path).unwrap();
    assert_eq!(loaded.logs.len(), 0);
    assert_eq!(loaded.slow_sqls.len(), 0);
    assert_eq!(loaded.sql_traces.len(), 0);
    assert!(loaded.collection_report.is_none());
}

// ─── 测试：NacosConfig default ───

#[test]
fn test_nacos_config_default() {
    let nc = NacosConfig::default();
    assert!(nc.address.is_empty());
    assert!(nc.namespace.is_empty());
}

// ─── 测试：build_quick_package 输出 Markdown SQL 文件 ───

#[test]
fn test_quick_package_outputs_markdown_sql() {
    use diag_core::package::build_quick_package;
    use std::io::Read as _;

    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("quick.zip");

    let logs = vec![LogEntry {
        time: Some("2026-06-17T10:00:00".into()),
        level: "DEBUG".into(),
        service: "pcm-user".into(),
        trace_id: Some("trace-001".into()),
        thread: Some("http-nio-8080-exec-3".into()),
        class: None,
        method: None,
        message: "查询用户权限".into(),
        exception: None,
        stack_trace: None,
        raw: "".into(),
    }];

    let sql_traces = vec![SqlTrace {
        trace_id: "trace-001".into(),
        service: "pcm-user".into(),
        sql: "select id, user_id from tb_user_permission where user_id = ? order by id".into(),
        sql_fingerprint: "select id, user_id from tb_user_permission where user_id = ? order by id"
            .into(),
        duration_ms: Some(1523.0),
        tables: vec!["tb_user_permission".into()],
        timestamp: Some("2026-06-17T10:00:00".into()),
        parameters: Some("218713736305705076(String)".into()),
    }];

    let explain_plans = vec![ExplainPlan {
        sql_fingerprint: "select id, user_id from tb_user_permission where user_id = ? order by id".into(),
        avg_duration_ms: 1523.0,
        source: "log_sql_explain".into(),
        explain_rows: vec![ExplainRow {
            id: Some(1),
            select_type: Some("SIMPLE".into()),
            table: Some("tb_user_permission".into()),
            access_type: Some("ref".into()),
            possible_keys: Some("idx_user_id".into()),
            key: Some("idx_user_id".into()),
            rows: Some(3),
            filtered: Some(100.0),
            extra: Some("Using where".into()),
        }],
        table_stats: None,
        trace_id: Some("trace-001".into()),
        executed_sql: Some("select id, user_id from tb_user_permission where user_id = '218713736305705076' order by id".into()),
        error: None,
        found_in_schema: None,
    }];

    let table_stats = vec![TableStats {
        schema: "pcm_2912".into(),
        table_name: "tb_user_permission".into(),
        row_count: 50000,
        data_size_bytes: Some(4194304),
        index_size_bytes: None,
        indexes: vec![IndexInfo {
            name: "idx_user_id".into(),
            columns: vec!["user_id".into()],
            unique: false,
        }],
    }];

    build_quick_package(&logs, &sql_traces, &explain_plans, &table_stats, &zip_path).unwrap();

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();

    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    assert!(
        names.contains(&"pcm-user_sql.md".to_string()),
        "expected pcm-user_sql.md, got: {:?}",
        names
    );
    assert!(names.contains(&"pcm-user.txt".to_string()));

    let mut md_content = String::new();
    archive
        .by_name("pcm-user_sql.md")
        .unwrap()
        .read_to_string(&mut md_content)
        .unwrap();

    // Markdown 头
    assert!(md_content.starts_with("# SQL 诊断报告"));
    // 拼装后的 SQL（参数已替换）
    assert!(
        md_content.contains("user_id = '218713736305705076'"),
        "executed sql with substituted param missing\n{}",
        md_content
    );
    // 表格元数据
    assert!(md_content.contains("| traceId |"));
    assert!(md_content.contains("| 涉及表 | tb_user_permission |"));
    // EXPLAIN 表格
    assert!(md_content.contains("| ref |"));
    assert!(md_content.contains("idx_user_id"));
    // 表统计
    assert!(md_content.contains("50,000"));
}
