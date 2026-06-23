use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone};
use diag_core::collector_trait::LogCollector;
use diag_core::config::{ServiceConfig, SshConfig};
use diag_core::log_parser;
use diag_core::models::{LogEntry, TimeWindow};

use crate::ssh_collector;

/// SSH 日志采集器 — 实现 LogCollector trait，通过 SSH grep 远端日志文件
pub struct SshLogCollector {
    ssh_config: SshConfig,
    services: Vec<ServiceConfig>,
    max_log_lines: usize,
}

impl SshLogCollector {
    pub fn new(ssh_config: SshConfig, services: Vec<ServiceConfig>, max_log_lines: usize) -> Self {
        Self {
            ssh_config,
            services,
            max_log_lines,
        }
    }

    fn find_service(&self, name: &str) -> Option<&ServiceConfig> {
        self.services.iter().find(|s| s.name == name)
    }
}

#[async_trait]
impl LogCollector for SshLogCollector {
    async fn query_by_trace_ids(
        &self,
        trace_ids: &[String],
        service: Option<&str>,
        window: &TimeWindow,
    ) -> Result<Vec<LogEntry>> {
        let services_to_query: Vec<&ServiceConfig> = match service {
            Some(name) => self.find_service(name).into_iter().collect(),
            None => self.services.iter().collect(),
        };
        let fetch_limit = query_line_limit(window, self.max_log_lines);

        let mut all_entries = Vec::new();
        for svc in services_to_query {
            for host in &svc.hosts {
                for trace_id in trace_ids {
                    match ssh_collector::grep_remote_logs(
                        host,
                        &self.ssh_config,
                        &svc.log_dir,
                        &svc.log_pattern,
                        trace_id,
                        fetch_limit,
                    )
                    .await
                    {
                        Ok(lines) => {
                            let entries: Vec<LogEntry> = lines
                                .iter()
                                .map(|line| log_parser::parse_log_line(line, &svc.name))
                                .collect();
                            for entry in filter_entries_by_window(entries, window)
                                .into_iter()
                                .take(self.max_log_lines)
                            {
                                all_entries.push(entry);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "SSH 采集 {}:{} traceId={} 失败: {}",
                                svc.name,
                                host,
                                trace_id,
                                e
                            );
                        }
                    }
                }
            }
        }
        Ok(filter_entries_by_window(all_entries, window))
    }

    async fn query_by_keywords(
        &self,
        keywords: &[String],
        service: Option<&str>,
        window: &TimeWindow,
    ) -> Result<Vec<LogEntry>> {
        let services_to_query: Vec<&ServiceConfig> = match service {
            Some(name) => self.find_service(name).into_iter().collect(),
            None => self.services.iter().collect(),
        };
        let fetch_limit = query_line_limit(window, self.max_log_lines);

        let mut all_entries = Vec::new();
        for svc in services_to_query {
            for host in &svc.hosts {
                for keyword in keywords {
                    match ssh_collector::grep_remote_logs(
                        host,
                        &self.ssh_config,
                        &svc.log_dir,
                        &svc.log_pattern,
                        keyword,
                        fetch_limit,
                    )
                    .await
                    {
                        Ok(lines) => {
                            let entries: Vec<LogEntry> = lines
                                .iter()
                                .map(|line| log_parser::parse_log_line(line, &svc.name))
                                .collect();
                            for entry in filter_entries_by_window(entries, window)
                                .into_iter()
                                .take(self.max_log_lines)
                            {
                                all_entries.push(entry);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "SSH 关键字采集 {}:{} 失败: {}",
                                svc.name,
                                host,
                                e
                            );
                        }
                    }
                }
            }
        }
        Ok(filter_entries_by_window(all_entries, window))
    }

    fn source_type(&self) -> &'static str {
        "ssh"
    }
}

fn filter_entries_by_window(entries: Vec<LogEntry>, window: &TimeWindow) -> Vec<LogEntry> {
    if window.start.is_empty() || window.end.is_empty() {
        return entries;
    }

    let Ok(start) = DateTime::parse_from_rfc3339(&window.start) else {
        return entries;
    };
    let Ok(end) = DateTime::parse_from_rfc3339(&window.end) else {
        return entries;
    };

    entries
        .into_iter()
        .filter(|entry| entry_in_window(entry, start, end))
        .collect()
}

fn query_line_limit(window: &TimeWindow, max_lines: usize) -> usize {
    if window.start.is_empty() || window.end.is_empty() {
        max_lines
    } else {
        max_lines.saturating_mul(5)
    }
}

fn entry_in_window(
    entry: &LogEntry,
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
) -> bool {
    match entry.time.as_deref().and_then(|time| {
        parse_entry_timestamp(time, *start.offset())
    }) {
        Some(timestamp) => timestamp >= start && timestamp <= end,
        None => true,
    }
}

fn parse_entry_timestamp(raw: &str, fallback_offset: FixedOffset) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(raw).ok().or_else(|| {
        [
            "%Y-%m-%d %H:%M:%S%.f",
            "%Y-%m-%dT%H:%M:%S%.f",
            "%Y-%m-%d %H:%M:%S",
            "%Y-%m-%dT%H:%M:%S",
        ]
        .iter()
        .find_map(|fmt| {
            NaiveDateTime::parse_from_str(raw, fmt)
                .ok()
                .and_then(|naive| fallback_offset.from_local_datetime(&naive).single())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_log(time: Option<&str>) -> LogEntry {
        LogEntry {
            time: time.map(str::to_string),
            level: "INFO".into(),
            service: "svc".into(),
            trace_id: Some("trace-1".into()),
            thread: None,
            class: None,
            method: None,
            message: "msg".into(),
            exception: None,
            stack_trace: None,
            raw: "raw".into(),
        }
    }

    #[test]
    fn test_filter_entries_by_window_applies_bounds_but_keeps_unknown_timestamps() {
        let entries = vec![
            sample_log(Some("2026-06-03T11:54:59+00:00")),
            sample_log(Some("2026-06-03T12:00:00+00:00")),
            sample_log(None),
        ];
        let window = TimeWindow {
            start: "2026-06-03T11:55:00+00:00".into(),
            end: "2026-06-03T12:05:00+00:00".into(),
        };

        let filtered = filter_entries_by_window(entries, &window);

        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].time.as_deref(), Some("2026-06-03T12:00:00+00:00"));
        assert_eq!(filtered[1].time, None);
    }

    #[test]
    fn test_filter_entries_by_window_filters_plain_text_log_timestamps() {
        let entries = vec![
            sample_log(Some("2026-06-03 11:54:59.000")),
            sample_log(Some("2026-06-03 12:00:00.000")),
        ];
        let window = TimeWindow {
            start: "2026-06-03T11:55:00+00:00".into(),
            end: "2026-06-03T12:05:00+00:00".into(),
        };

        let filtered = filter_entries_by_window(entries, &window);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].time.as_deref(), Some("2026-06-03 12:00:00.000"));
    }

    #[test]
    fn test_query_line_limit_expands_when_window_present() {
        let window = TimeWindow {
            start: "2026-06-03T11:55:00+00:00".into(),
            end: "2026-06-03T12:05:00+00:00".into(),
        };

        assert_eq!(query_line_limit(&window, 100), 500);
    }
}
