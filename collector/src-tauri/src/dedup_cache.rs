use std::collections::HashMap;
use std::time::{Duration, Instant};

/// traceId 去重缓存，防止同一 traceId 在短时间内反复触发采集
pub struct DedupCache {
    seen: HashMap<String, Instant>,
    window: Duration,
}

impl DedupCache {
    pub fn new(window_minutes: u32) -> Self {
        Self {
            seen: HashMap::new(),
            window: Duration::from_secs(window_minutes as u64 * 60),
        }
    }

    /// 检查 traceId 是否在去重窗口内已被处理过
    pub fn contains(&self, trace_id: &str) -> bool {
        if let Some(seen_at) = self.seen.get(trace_id) {
            seen_at.elapsed() < self.window
        } else {
            false
        }
    }

    /// 标记 traceId 为已处理
    pub fn insert(&mut self, trace_id: &str) {
        self.seen.insert(trace_id.to_string(), Instant::now());
    }

    /// 批量标记
    pub fn insert_all(&mut self, trace_ids: &[String]) {
        let now = Instant::now();
        for id in trace_ids {
            self.seen.insert(id.clone(), now);
        }
    }

    /// 清理过期条目（防内存无限增长）
    pub fn prune_expired(&mut self) {
        self.seen
            .retain(|_, seen_at| seen_at.elapsed() < self.window);
    }

    /// 从列表中过滤掉已在缓存中的 traceId
    pub fn filter_new<'a>(&self, trace_ids: &'a [String]) -> Vec<&'a String> {
        trace_ids.iter().filter(|id| !self.contains(id)).collect()
    }
}
