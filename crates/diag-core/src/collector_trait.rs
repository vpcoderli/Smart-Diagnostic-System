use crate::models::{LogEntry, ServiceInstance, TimeWindow};
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait LogCollector: Send + Sync {
    /// 按 traceId 列表查询日志
    async fn query_by_trace_ids(
        &self,
        trace_ids: &[String],
        service: Option<&str>,
        window: &TimeWindow,
    ) -> Result<Vec<LogEntry>>;

    /// 按关键词查询日志（用于历史模式和定时巡检）
    async fn query_by_keywords(
        &self,
        keywords: &[String],
        service: Option<&str>,
        window: &TimeWindow,
    ) -> Result<Vec<LogEntry>>;

    fn source_type(&self) -> &'static str;

    fn warnings(&self) -> Vec<String> {
        Vec::new()
    }
}

#[async_trait]
pub trait ServiceDiscovery: Send + Sync {
    async fn discover_services(&self, prefix: &str) -> Result<Vec<ServiceInstance>>;
    fn source_type(&self) -> &'static str;
}
