use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tauri::Emitter;
use tokio::sync::oneshot;

use diag_core::config::CollectorConfig;
use diag_core::models::TimeWindow;

use crate::dedup_cache::DedupCache;

/// 调度器运行状态（供前端查询）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerStatus {
    pub running: bool,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub total_runs: usize,
    pub packages_created: usize,
    pub last_errors: Vec<String>,
    pub recent_packages: Vec<PackageInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageInfo {
    pub file_name: String,
    pub trace_count: usize,
    pub created_at: String,
}

/// 调度器句柄（持有后台任务的控制权）
pub struct SchedulerHandle {
    pub shutdown_tx: Option<oneshot::Sender<()>>,
}

impl SchedulerHandle {
    /// 发送停止信号
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// 启动调度器后台任务
///
/// 返回 `SchedulerHandle`（用于后续停止）和共享的 `SchedulerStatus`。
pub fn start(
    app: tauri::AppHandle,
    config: CollectorConfig,
    status: Arc<Mutex<SchedulerStatus>>,
) -> Result<SchedulerHandle> {
    let schedule = config
        .schedule
        .clone()
        .ok_or_else(|| anyhow::anyhow!("未配置定时任务参数"))?;

    if config.elk.is_none() {
        return Err(anyhow::anyhow!("定时巡检需要 ELK 配置，当前未配置 ELK"));
    }

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    // 设置初始状态
    {
        let mut s = status.lock().unwrap();
        s.running = true;
        s.total_runs = 0;
        s.packages_created = 0;
        s.last_errors.clear();
        s.next_run_at = Some(
            (Utc::now() + chrono::Duration::minutes(schedule.interval_minutes as i64)).to_rfc3339(),
        );
    }

    let status_clone = status.clone();
    tokio::spawn(async move {
        scheduler_loop(app, config, status_clone, shutdown_rx).await;
    });

    Ok(SchedulerHandle {
        shutdown_tx: Some(shutdown_tx),
    })
}

async fn scheduler_loop(
    app: tauri::AppHandle,
    config: CollectorConfig,
    status: Arc<Mutex<SchedulerStatus>>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let schedule = match &config.schedule {
        Some(s) => s.clone(),
        None => return,
    };

    let interval_secs = schedule.interval_minutes as u64 * 60;
    let mut timer = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 跳过第一次立即触发（启动时不立即采集）
    timer.tick().await;

    let mut dedup = DedupCache::new(schedule.dedup_window_minutes);

    tracing::info!(
        "调度器启动：每 {} 分钟巡检一次，回看 {} 分钟，级别: {:?}",
        schedule.interval_minutes,
        schedule.lookback_minutes,
        schedule.levels
    );

    loop {
        // 更新 next_run_at
        {
            let mut s = status.lock().unwrap();
            s.next_run_at = Some(
                (Utc::now() + chrono::Duration::minutes(schedule.interval_minutes as i64))
                    .to_rfc3339(),
            );
        }

        tokio::select! {
            _ = timer.tick() => {
                // 清理过期去重条目
                dedup.prune_expired();

                let run_result = run_scheduled_check(
                    &app,
                    &config,
                    &mut dedup,
                ).await;

                let now = Utc::now().to_rfc3339();
                let mut s = status.lock().unwrap();
                s.total_runs += 1;
                s.last_run_at = Some(now.clone());

                match run_result {
                    Ok(Some(pkg_info)) => {
                        s.packages_created += 1;
                        s.recent_packages.push(pkg_info);
                        if s.recent_packages.len() > 10 {
                            s.recent_packages.remove(0);
                        }
                        if !s.last_errors.is_empty() {
                            s.last_errors.clear();
                        }
                        tracing::info!("巡检完成：本轮生成 1 个诊断包");
                        // 触发旧包清理（在独立线程中执行，不阻塞调度器）
                        if let Some(ref sched) = config.schedule {
                            let output_dir = config.collector.output_dir.clone();
                            let retention = sched.output_retention_days;
                            tokio::task::spawn_blocking(move || {
                                crate::cleanup::cleanup_old_packages(&output_dir, retention);
                            });
                        }
                    }
                    Ok(None) => {
                        tracing::debug!("巡检完成：本轮无新包");
                    }
                    Err(e) => {
                        let err_msg = e.to_string();
                        tracing::warn!("巡检出错: {}", err_msg);
                        s.last_errors.push(err_msg);
                        if s.last_errors.len() > 5 {
                            s.last_errors.remove(0);
                        }
                    }
                }
            }
            _ = &mut shutdown_rx => {
                tracing::info!("调度器已停止");
                break;
            }
        }
    }

    let mut s = status.lock().unwrap();
    s.running = false;
    s.next_run_at = None;
}

/// 单次巡检：查 ELK → 过滤去重 → 诊断 → 返回生成的包信息
async fn run_scheduled_check(
    app: &tauri::AppHandle,
    config: &CollectorConfig,
    dedup: &mut DedupCache,
) -> Result<Option<PackageInfo>> {
    let schedule = config.schedule.as_ref().unwrap();
    let elk_config = config.elk.as_ref().unwrap();

    // 1. 计算查询时间窗口
    let now = Utc::now();
    let lookback_secs = (schedule.lookback_minutes + schedule.overlap_minutes) as i64 * 60;
    let window = TimeWindow {
        start: (now - chrono::Duration::seconds(lookback_secs)).to_rfc3339(),
        end: now.to_rfc3339(),
    };

    tracing::debug!("巡检时间窗口: {} ~ {}", window.start, window.end);

    // 2. 构建 ELK 采集器
    let elk_collector = crate::elk_collector::ElkCollector::new(elk_config.clone())
        .await
        .map_err(|e| anyhow::anyhow!("ELK 连接失败: {}", e))?;

    // 3. 查询 ERROR/WARN 日志
    let logs = elk_collector
        .query_by_levels(&schedule.levels, None, &window, &schedule.extra_keywords)
        .await
        .map_err(|e| anyhow::anyhow!("ELK 查询失败: {}", e))?;

    if logs.is_empty() {
        tracing::debug!("本轮无 ERROR/WARN 日志，跳过");
        return Ok(None);
    }

    tracing::info!("本轮查到 {} 条 ERROR/WARN 日志", logs.len());

    // 4. 提取 traceId 并去重
    let all_trace_ids: Vec<String> = logs
        .iter()
        .filter_map(|l| l.trace_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let new_trace_ids: Vec<String> = dedup
        .filter_new(&all_trace_ids)
        .into_iter()
        .cloned()
        .take(schedule.max_trace_ids_per_run)
        .collect();

    if new_trace_ids.is_empty() {
        tracing::debug!("本轮所有 traceId 均已处理过，跳过");
        return Ok(None);
    }

    tracing::info!("本轮新 traceId {} 个（过滤已处理）", new_trace_ids.len());

    // 5. 运行历史模式诊断
    let log_collector: Box<dyn diag_core::collector_trait::LogCollector> =
        Box::new(crate::elk_collector::ElkCollector::new(elk_config.clone()).await?);

    let runner = crate::diagnosis::DiagnosisRunner::new_historical(
        config.clone(),
        log_collector,
        new_trace_ids.clone(),
    );

    let output_path = runner
        .run()
        .await
        .map_err(|e| anyhow::anyhow!("诊断执行失败: {}", e))?;

    // 6. 标记已处理
    dedup.insert_all(&new_trace_ids);

    let file_name = std::path::Path::new(&output_path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| output_path.clone());

    // 7. 通知前端（Tauri 事件）
    let _ = app.emit(
        "scheduler-package-created",
        serde_json::json!({
            "outputPath": output_path,
            "fileName": file_name,
            "traceCount": new_trace_ids.len(),
            "logCount": logs.len(),
            "timestamp": Utc::now().to_rfc3339(),
        }),
    );

    Ok(Some(PackageInfo {
        file_name,
        trace_count: new_trace_ids.len(),
        created_at: Utc::now().to_rfc3339(),
    }))
}
