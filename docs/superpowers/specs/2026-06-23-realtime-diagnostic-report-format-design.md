 xianz# 实时诊断报告格式设计

## 背景

当前实时诊断已经能够收集浏览器请求、trace 日志、SQL、执行计划和表统计，但主要输出仍偏“材料归档”：日志按服务文件展示，SQL 另按服务生成 Markdown。开发或运维排查线上问题时，需要在请求、日志、SQL、执行计划之间来回跳转，定位单个 trace 的完整上下文不够直观。

用户提供的目标样例将 `Request URL`、状态码、时间窗口、日志和 SQL 执行计划放在同一个 trace 下。这个方向更符合线上排查习惯，但需要补充风险摘要、日志分层、SQL 与 EXPLAIN 的一对一关联，才能成为稳定的排查报告格式。

## 目标

- 让开发/运维以“一个请求就是一个事故现场”的方式阅读实时诊断结果。
- 在 Markdown/ZIP 内提供清晰入口：先总览定位高风险请求，再进入单个 trace 深挖。
- 每个请求卡片内串联请求信息、初步判断、关键日志、相关 SQL、执行计划、表和索引信息。
- 保留原始 service 维度日志和 SQL 文件作为证据归档，但不再作为主阅读入口。

## 非目标

- 不在本设计中调整采集链路、ELK 查询逻辑或数据库连接逻辑。
- 不引入前端页面可视化交互；本设计优先服务 ZIP 内 Markdown 报告。
- 不替代 analyzer 的规则引擎报告；实时报告重点是采集端的原始问题现场组织。

## 推荐方案

采用“总览索引 + 单请求排查卡片 + 原始证据归档”的三层结构。

### 1. 总览索引

新增或生成 `realtime/overview.md`，作为实时诊断结果的主入口。它用表格列出每个浏览器捕获请求的核心排查信号：

```md
# 实时诊断报告

## 一、问题总览

| # | 风险 | traceId | 接口 | 状态码 | 耗时 | 服务 | 日志信号 | SQL | EXPLAIN |
|---|------|---------|------|--------|------|------|----------|-----|---------|
| 1 | ERROR | `9510ac35ab4541df95cdfbd42eca13eb` | /v1/pt/hospital/outline/new/taskDetail | 500 | 1200ms | pcm-statistics | ERROR=1 WARN=2 | 3 | 2 成功 / 1 失败 |
```

风险等级建议沿用现有 analyzer 阈值语义：HTTP 500+ 为 ERROR，4xx 或超过 1000ms 为 WARN，超过 2000ms 为 SLOW；如果后续实时侧已有更细规则，可在实现计划中对齐。

### 2. 单请求排查卡片

新增或生成 `realtime/request-cards.md`，每个请求独立成节。推荐固定结构如下：

```md
## 1. [ERROR] x-trace：`xxxxx`

### 请求信息

| 项目 | 值 |
|------|-----|
| Request URL | `http://172.29.193.157:8001/gateway/pcm-statistics/v1/pt/hospital/outline/new/taskDetail` |
| Method / Status | GET / 500 |
| startTime | 2026-06-23T01:57:54.923Z |
| endTime | 2026-06-23T01:57:56.100Z |
| duration | 1177 ms |
| 入口服务 | pcm-statistics |
| 关联服务 | pcm-statistics, pcm-management |

### 初步判断

- 结论：接口异常 / 慢请求 / 仅日志告警 / 暂无明显异常
- 主要证据：HTTP 500、ERROR 日志、慢 SQL、EXPLAIN 全表扫描
- 建议优先排查：服务、SQL、表、索引或权限问题

### 关键日志

```text
按时间排序展示该 trace 下关键日志
```

### 相关 SQL 与执行计划

#### SQL 1：outbound_platform.tb_name_list

| 项目 | 值 |
|------|-----|
| traceId | `xxxxx` |
| 服务 | outbound-common-manager |
| 时间 | 2026-06-23T01:57:54.923Z |
| 涉及表 | outbound_platform.tb_name_list |
| 参数状态 | 已拼装 / 参数缺失 |
| EXPLAIN 状态 | 成功 / 失败 |

```sql
SELECT count(0)
FROM "tb_name_list"
WHERE "TYPE" = 'patient'
  AND "NAME" LIKE CONCAT('%', '张三', '%')
```

**表数据量与索引：**

| 表名 | 行数 | 数据大小 | 索引数 | 索引列表 |
|------|------|----------|--------|----------|

**执行计划：**

```text
EXPLAIN 结果或明确失败原因
```

### 完整证据

- 完整服务日志：`pcm-management.txt`
- 服务 SQL 报告：`pcm-management_sql.md`
```

### 3. 原始证据归档

继续保留现有 service 维度文件，例如：

- `pcm-management.txt`
- `pcm-management_sql.md`
- 现有结构化 JSON 包内容

这些文件用于审计、复核和二次分析。主报告只引用它们，不把它们作为第一阅读入口。

## 展示规则

### 日志

- 卡片内日志按时间排序。
- 优先展示关键日志：`ERROR`、`WARN`、异常栈、`RequestUrl`、SQL `Preparing`、`Parameters`、`Total`。
- 如日志量较大，卡片展示关键日志，完整日志保留在 service 原始文件。
- 未匹配浏览器请求 traceId 的日志放入独立“未匹配请求日志”区域，避免混入某个请求卡片。

### SQL 与执行计划

- SQL 与对应 EXPLAIN 必须在同一小节内展示。
- 每条 SQL 展示参数状态：已拼装、参数缺失、仍存在占位符。
- PostgreSQL 表名展示为 `schema.table`，避免同名表跨 schema 时误判。
- EXPLAIN 失败必须给出可操作原因，例如参数缺失、schema 未发现、权限不足或 SQL 方言不兼容。

### 风险摘要

每个请求卡片必须有“初步判断”，避免报告只堆证据。判断只做辅助定位，不替代人工结论。推荐输出：

- 风险等级：ERROR / SLOW / WARN / OK
- 主要证据：状态码、耗时、错误日志、慢 SQL、执行计划风险
- 建议优先排查对象：服务、SQL、表、索引、权限或配置

## 数据流

1. WebView 捕获浏览器请求，得到 URL、状态码、耗时、traceId 和时间。
2. 诊断流程按 traceId 查询日志，并提取 SQL。
3. SQL 关联 EXPLAIN 结果和表统计。
4. 报告生成阶段以浏览器请求为主轴聚合数据：
   - request -> traceId
   - traceId -> logs
   - traceId -> sql traces
   - sql fingerprint -> explain plans
   - table name/schema -> table stats
5. Markdown 输出总览索引和请求卡片。
6. 原始 service 日志和 SQL 文件继续写入 ZIP。

## 错误与缺失数据处理

- 请求无 traceId：卡片仍输出请求信息，并提示“无 traceId，无法关联日志”。
- traceId 无日志：卡片提示“未查询到该 traceId 的日志”。
- SQL 无参数：展示原 SQL，并标记参数状态。
- EXPLAIN 失败：展示失败原因，不隐藏 SQL。
- 表统计缺失：展示 `-`，并提示可能是权限、schema 或采集范围问题。
- 未匹配请求的日志：放入独立区域，标明未能与浏览器请求关联。

## 测试策略

- 构造包含多个浏览器请求和多个 trace 的实时包，验证每个请求卡片只展示自身 trace 的日志。
- 构造同一 trace 下日志、SQL、EXPLAIN、表统计完整数据，验证它们出现在同一卡片内。
- 构造 PostgreSQL 同名表跨 schema 数据，验证报告展示 `schema.table`。
- 构造 EXPLAIN 失败、参数缺失、无日志和无 traceId 场景，验证报告给出明确提示。
- 保留现有 service 维度输出测试，确保主报告增强不破坏原始归档兼容性。

## 结论

用户样例比当前分散输出更适合问题分析，但最优形态应升级为“总览索引 + 单请求排查卡片”。这种格式遵循线上排查路径：先定位高风险请求，再围绕一个 trace 查看请求、日志、SQL、执行计划和表信息，最后回到原始证据复核。
