# 收集端 v2.0 演进设计

> 日期：2026-05-26 | 状态：设计确认 | 范围：smart-diag-collector 收集端

---

## 1. 背景与目标

MVP（v1.0）收集端已实现：WebView JS 注入抓包 → SSH grep 日志 → DB 慢 SQL 查询 → 脱敏打包。

v2.0 演进目标：

| 维度 | v1.0 现状 | v2.0 目标 |
|---|---|---|
| 日志采集 | SSH grep 日志文件 | ELK 为主，SSH 为降级兜底 |
| 服务发现 | CSV 手动导入 | Nacos 注册表自动发现，CSV 降级 |
| SQL 分析 | 全局慢 SQL 统计 | 应用日志 SQL + traceId 关联 + EXPLAIN |
| 采集模式 | 仅实时复现 | 实时复现 + 历史关键字回溯 |

**本轮设计范围：收集端。分析端不在本次讨论范围内。**

---

## 2. 双模式采集

### 2.1 模式一：实时复现模式（保留现有能力）

适用场景：问题可以复现，需要实时抓取完整请求链路。

```
运维输入页面 URL
    → 内嵌浏览器打开，JS 注入拦截 fetch/XHR
    → 运维操作复现问题
    → 点击「采集完成」
    → 从拦截到的请求中提取 traceId 列表
    → 用 traceId 去 ELK 精确查询日志（降级：SSH grep）
    → 从日志中解析 SQL → 与 DB 慢日志交叉 → EXPLAIN
    → 脱敏打包 → 下载 diagnosis.zip
```

### 2.2 模式二：历史回溯模式（新增）

适用场景：问题已发生无法复现，或需要回溯历史问题。

```
运维输入关键字（错误信息 / 患者ID / 操作时间范围）
    → ELK 全文搜索，返回匹配日志 + traceId 列表
    → 按 traceId 拉取完整链路日志
    → 从日志中解析 SQL → 与 DB 慢日志交叉 → EXPLAIN
    → 脱敏打包 → 下载 diagnosis.zip
```

两种模式共用同一条诊断流水线（从"拿到 traceId 列表"之后完全一致）。

---

## 3. 服务发现层（三级降级）

```
优先级 1：Nacos 注册表
  GET /nacos/v1/ns/instance/list?serviceName={service}&healthy=true
  → 获取实例 IP 列表
  → 日志路径按约定规则推断：/var/log/{service-name}/
  → 支持手动覆盖单个服务的日志路径

优先级 2：CSV 导入（现有能力）
  → 无 Nacos 或 Nacos 不可达时降级

优先级 3：手动输入
  → 最后兜底
```

**Nacos 配置项：**

```toml
[nacos]
address = "http://172.29.60.200:8848"
namespace = "pcm-prod"
group = "DEFAULT_GROUP"
service_prefix = "pcm-"          # 只发现 pcm-* 服务
log_path_pattern = "/var/log/{service-name}/"  # 约定路径规则
```

**站点配置持久化：** 初次由研发配置完成后，导出为 `site-config.json`，运维下次打开直接加载，无需重新配置。

---

## 4. ELK 日志采集层

### 4.1 版本适配

```
启动时检测：GET /_cluster/health → 解析 version.number
  ES 6.x → 使用 query_string 语法
  ES 7/8  → 使用 bool/must/match 语法
```

### 4.2 字段规范

公司内部统一，固定字段名（不需要配置映射）：

| 字段 | ES 字段名 |
|---|---|
| traceId | `traceId` |
| 日志级别 | `level` |
| 服务名 | `serviceName` |
| 时间戳 | `@timestamp` |

日志索引名可配置，默认 `app-logs-*`。

### 4.3 两种查询策略

**精确查询（实时复现模式）：**
```json
{
  "query": {
    "bool": {
      "must": [
        { "term": { "traceId": "<traceId>" } },
        { "range": { "@timestamp": { "gte": "<start>", "lte": "<end>" } } }
      ]
    }
  }
}
```

**全文查询（历史回溯模式）：**
```json
{
  "query": {
    "bool": {
      "must": [
        { "query_string": { "query": "<关键字>" } },
        { "range": { "@timestamp": { "gte": "<start>", "lte": "<end>" } } }
      ]
    }
  }
}
```

### 4.4 降级策略

```
ELK 查询失败（连接超时 / 索引不存在 / 认证失败）
    → 自动降级到 SSH grep
    → UI 显示降级提示："ELK 不可用，已切换为 SSH 采集"
```

---

## 5. SQL 深化采集层

### 5.1 数据来源与交叉分析

```
来源一：应用日志中的 SQL（有 traceId 关联）
  → 从 ELK / SSH 拿到的日志里，按正则提取 MyBatis/Hibernate 打印的 SQL
  → 记录：traceId + SQL 原文 + 执行时间（如日志中有）

来源二：DB 慢日志（全局统计，现有能力）
  → MySQL: performance_schema.events_statements_summary_by_digest
  → PostgreSQL: pg_stat_statements

交叉分析：
  对两个来源的 SQL 做 fingerprint（参数替换为 ?）
  → fingerprint 匹配：认定为"链路慢查询"，执行 EXPLAIN
  → 仅在应用日志中出现：记录 fingerprint，不执行 EXPLAIN
  → 仅在慢日志中出现：记录统计数据，不执行 EXPLAIN
```

### 5.2 EXPLAIN 分层策略

```
慢查询（avg_duration > 500ms，可配置阈值）：
  → 收集端实时执行 EXPLAIN，结果写入诊断包
  → 同时采集涉及表的 information_schema 统计（行数、索引定义）

普通 SQL：
  → 只记录 fingerprint，不执行 EXPLAIN
  → 留给分析端判断是否需要深挖
```

**EXPLAIN 采集内容：**

```json
{
  "sql_fingerprint": "SELECT * FROM patient WHERE id = ?",
  "avg_duration_ms": 1200,
  "explain": {
    "select_type": "SIMPLE",
    "table": "patient",
    "type": "ALL",
    "possible_keys": null,
    "key": null,
    "rows": 850000,
    "extra": "Using where"
  },
  "table_stats": {
    "table_name": "patient",
    "row_count": 850000,
    "data_size_mb": 420
  }
}
```

---

## 6. 诊断包格式升级

```
diagnosis-{site}-{timestamp}.zip
├── manifest.json                    # 新增字段：collectionMode, elkVersion
├── browser/
│   └── requests.json                # 仅实时复现模式有，历史模式此目录为空
├── services/
│   └── {service-name}/
│       ├── app-log.jsonl            # ELK / SSH 采集的日志（格式不变）
│       └── sql-trace.jsonl          # 新增：从日志中提取的 SQL + traceId
├── database/
│   ├── slow-sql.json                # 现有：DB 慢日志统计
│   ├── table-stats.json             # 现有：表行数 / 索引信息
│   └── explain-plans.json           # 新增：慢查询的 EXPLAIN 结果
└── privacy/
    └── masking-report.json
```

**manifest.json 新增字段：**

```json
{
  "collectionMode": "realtime | historical",
  "elkVersion": "7.17.0",
  "logSource": "elk | ssh | mixed",
  "keywords": ["患者ID:12345", "NullPointerException"],
  "timeRange": { "start": "2026-05-26T10:00:00+08:00", "end": "2026-05-26T10:30:00+08:00" }
}
```

---

## 7. 代码架构调整

### 7.1 新增 trait 抽象（为未来插件化打基础）

在 `diag-core` 中定义采集接口：

```rust
// crates/diag-core/src/collector.rs

#[async_trait]
pub trait LogCollector: Send + Sync {
    async fn query_by_trace_ids(&self, trace_ids: &[String], window: &TimeWindow) -> Result<Vec<LogEntry>>;
    async fn query_by_keywords(&self, keywords: &[String], window: &TimeWindow) -> Result<Vec<LogEntry>>;
    fn source_type(&self) -> &'static str;
}

#[async_trait]
pub trait ServiceDiscovery: Send + Sync {
    async fn list_services(&self) -> Result<Vec<ServiceInstance>>;
    fn source_type(&self) -> &'static str;
}
```

### 7.2 新增模块（collector/src-tauri/src/）

| 模块 | 职责 |
|---|---|
| `elk_collector.rs` | 实现 `LogCollector` trait，ES 6/7/8 版本适配 |
| `nacos_discovery.rs` | 实现 `ServiceDiscovery` trait，Nacos 注册表查询 |
| `sql_extractor.rs` | 从日志文本中提取 SQL 语句，fingerprint 化 |
| `explain_collector.rs` | 分层 EXPLAIN 策略，MySQL / PostgreSQL 适配 |

### 7.3 现有模块调整

| 模块 | 调整内容 |
|---|---|
| `diagnosis.rs` | DiagnosisRunner 接收 `Box<dyn LogCollector>`，支持 ELK/SSH 切换 |
| `commands.rs` | 新增 `start_historical_diagnosis` 命令，新增 `discover_from_nacos` 命令 |
| `config_store.rs` | 新增 Nacos 配置项、ELK 配置项、站点配置导出/导入 |

---

## 8. UI 流程调整

### 主界面新增模式选择

```
┌─────────────────────────────────────┐
│  选择采集模式                         │
│  ○ 实时复现  ● 历史回溯               │
└─────────────────────────────────────┘
```

### 历史回溯模式输入区

```
关键字：[患者ID / 错误信息 / 接口路径    ]
时间范围：[2026-05-26 10:00] 至 [2026-05-26 10:30]
[开始采集]
```

### 配置页新增

- Nacos 地址 + 命名空间 + 服务前缀
- ELK 地址 + 索引名 + 认证信息
- 日志路径约定规则（可覆盖单个服务）
- 导出站点配置 / 导入站点配置

---

## 9. 关键约束与 MVP 边界

- **单次采集时间窗口**：默认 30 分钟，可配置，最大 2 小时（防止 ELK 返回数据量过大）
- **ELK 返回日志条数上限**：每个 traceId 最多 1000 条，全文搜索最多 5000 条
- **EXPLAIN 执行上限**：单次诊断最多执行 20 条 EXPLAIN（防止对生产 DB 产生压力）
- **本轮不包含**：分析端的执行计划可视化、规则引擎增强（下一轮设计）
