# Smart Diagnostic System

> 面向 toB 医疗项目的双端离线诊断工具。解决开发人员无法直接访问医院内网时，难以复现和定位线上问题的核心痛点。

---

## 目录

- [项目背景](#项目背景)
- [系统架构](#系统架构)
- [功能特性](#功能特性)
- [快速开始](#快速开始)
- [配置说明](#配置说明)
- [诊断报告格式](#诊断报告格式)
- [构建与发布](#构建与发布)
- [项目结构](#项目结构)
- [关键约定](#关键约定)
- [隐私与安全](#隐私与安全)

---

## 项目背景

医院内网环境封闭，开发人员无法直接 SSH 或访问数据库，线上问题复现成本极高。本系统通过两个桌面端应用将"采集"与"分析"解耦：

- **收集端** 部署在医院内网的 Windows 跳板机上，负责在现场采集数据并打包
- **分析端** 部署在开发人员电脑上，读取诊断包进行离线分析

---

## 系统架构

```
[医院内网]                                          [开发人员电脑]
┌────────────────────────────────────┐              ┌──────────────────────────────────┐
│   smart-diag-collector (收集端)     │              │   smart-diag-analyzer (分析端)    │
│                                    │              │                                  │
│  阶段一：导入 CSV 部署文档           │              │  1. 导入 diagnosis.zip            │
│  阶段二：校验 SSH / DB / ELK 连接   │  diagnosis   │  2. 运行规则引擎                  │
│  阶段三：诊断采集（三种模式）         │  .zip ─────► │  3. 导出 Markdown 分析报告        │
│    实时 / 历史 / 定时巡检           │              │                                  │
└────────────────────────────────────┘              └──────────────────────────────────┘
```

### Workspace 结构

| Crate | 说明 |
|---|---|
| `crates/diag-core` | 共享库：数据模型、日志/SQL 解析器、隐私脱敏、ZIP 打包、采集器 Trait |
| `collector/src-tauri` | 收集端 Tauri 应用后端 |
| `analyzer/src-tauri` | 分析端 Tauri 应用后端 |

---

## 功能特性

### 收集端

| 功能 | 说明 |
|---|---|
| **三步向导** | 导入部署文档 → 连接校验 → 诊断采集 |
| **实时诊断** | 通过内嵌诊断浏览器注入 JS，拦截所有 `fetch`/`XHR` 请求，复现问题时自动关联 traceId |
| **历史诊断** | 输入 traceId 列表 + 时间窗口，直接查询 ELK 或 SSH 日志 |
| **定时巡检** | 后台定时（默认每 5 分钟）扫描 ELK 错误日志，自动去重后打包 |
| **双日志源** | ELK（Elasticsearch/Kibana 代理，兼容 6.x/7.x/8.x）或 SSH grep，可按需切换 |
| **慢 SQL 采集** | 查询 MySQL `performance_schema` 或 PostgreSQL `pg_stat_statements` 慢查询统计 |
| **EXPLAIN 执行计划** | 自动对慢 SQL 运行 `EXPLAIN`，分析全表扫描风险 |
| **隐私脱敏** | URL 查询参数自动脱敏（`***`），白名单保留 `pageNum/pageSize` 等业务参数 |
| **配置持久化** | 部署配置自动保存，下次启动免重新导入 |

### 分析端

| 功能 | 说明 |
|---|---|
| **规则引擎** | HTTP 状态码、响应耗时、日志级别、SQL 执行时间、执行计划扫描放大比等多维度风险判断 |
| **Markdown 报告** | 导出按 traceId 组织的结构化分析报告 |

### 风险阈值

| 类型 | ERROR | SLOW | WARN |
|---|---|---|---|
| HTTP | 状态码 ≥ 500 | 耗时 > 2000ms | 耗时 > 1000ms 或 4xx |
| SQL | — | 平均耗时 > 1000ms | 扫描放大 > 100x |

---

## 快速开始

### 环境要求

- Rust stable 工具链（推荐通过 [rustup](https://rustup.rs) 安装）
- Node.js ≥ 18（Tauri 打包器依赖，前端无构建步骤）
- Tauri CLI：`cargo install tauri-cli --version "^2"`
- 收集端：需要 Windows 跳板机可访问目标服务器 SSH + 数据库 + ELK（三者可选配）

### 开发模式运行

```sh
# 收集端
cd collector/src-tauri && cargo tauri dev

# 分析端
cd analyzer/src-tauri && cargo tauri dev

# 仅编译检查（无 GUI，从 workspace 根目录）
cargo check
cargo build -p diag-core
cargo build -p smart-diag-collector
```

### 使用流程（收集端）

**阶段一：导入部署文档**

1. 点击「下载服务模板」，填写各微服务的 IP、日志路径等信息（CSV 格式）
2. 点击「下载数据库模板」，填写数据库连接信息
3. 导入两个 CSV 文件，填写站点名称

**阶段二：校验连接**

4. 逐一测试 SSH 连接，确认可访问目标服务器日志目录
5. 测试数据库连接，选择要监控的 Schema / 数据库
6. （可选）配置 ELK 连接信息并测试

**阶段三：诊断采集**

7. 选择采集模式：
   - **实时诊断**：点击「打开诊断浏览器」→ 复现问题 → 点击「开始诊断」
   - **历史诊断**：输入 traceId 列表 + 时间范围 → 开始诊断
   - **定时巡检**：配置巡检间隔，启动后台调度器自动运行
8. 诊断完成后，`diagnosis.zip` 自动保存到输出目录

**分析端**

9. 将 `diagnosis.zip` 拷贝到开发机
10. 打开分析端，导入 ZIP，查看分析报告

---

## 配置说明

收集端通过 GUI 向导导入 CSV 自动生成配置，也支持手动编写 TOML 配置文件。

参考 [`collector/collector.toml.example`](collector/collector.toml.example)：

```toml
[site]
name = "hospital-a"         # 站点标识，用于报告命名
system = "pcm"              # 业务系统标识

[gateway]
prefix = "/gateway"         # 网关前缀，用于 URL 解析

[ssh]
port = 22
username = "deploy"
auth_type = "password"      # "password" 或 "key"
password = "your_password"
# private_key = "C:\\Users\\admin\\.ssh\\id_rsa"  # key 模式时使用

[database]
type = "mysql"              # "mysql" 或 "postgresql"
host = "172.29.60.100"
port = 3306
username = "readonly"
password = "readonly_password"
database = "pcm_db"
# schemas = ["public", "pcm"]  # PostgreSQL 专用

[privacy]
mask_query_values = true
allowed_query_keys = ["pageNum", "pageSize", "portal", "type", "status"]

[collector]
time_window_minutes = 30    # 日志查询时间窗口
max_log_lines = 500         # 每个 trace 最大日志行数
output_dir = "C:\\diagnosis\\output"

[[services]]
name = "pcm-server"
display = "患者管理服务"
hosts = ["172.29.60.10"]
log_dir = "/opt/pcm/pcm-server/logs/"
log_pattern = "*.log"
```

### ELK 配置（可选）

```toml
[elk]
address = "http://elk.internal:9200"
index_pattern = "logstash-*"
username = "elastic"
password = "your_password"
timeout_secs = 30
max_hits_per_trace = 1000

[elk.field_mapping]
timestamp = "@timestamp"
level = "level"
service = "serviceName"
trace_id = "traceId"
message = "message"
```

> **ELK 字段映射**：默认适配标准 Logstash 输出格式，如日志字段名不同可在 `field_mapping` 中逐一修改。

### 定时巡检配置（可选）

```toml
[schedule]
enabled = true
interval_minutes = 5        # 巡检间隔（分钟）
lookback_minutes = 6        # 每次回溯时长
overlap_minutes = 1         # 防漏窗口重叠
levels = ["ERROR", "WARN"]  # 监控日志级别
max_trace_ids_per_run = 50  # 单次最多处理 traceId 数
dedup_window_minutes = 60   # 去重时间窗口
output_retention_days = 7   # 诊断包保留天数
```

---

## 诊断报告格式

`diagnosis.zip` 解压后结构如下：

```
diagnosis-YYYYMMDD-HHMMSS-<page-slug>.zip
├── manifest.json                  # 诊断元数据（站点、时间、traceId 列表等）
├── realtime-report.txt            # 主报告（总览索引 + 单请求排查卡片）
├── browser/
│   └── requests.json              # 浏览器捕获的 HTTP 请求列表
└── services/
    └── {service-name}/
        └── app-log.txt            # 完整服务日志（含 SQL、EXPLAIN）
```

### 主报告结构（realtime-report.txt）

报告分三层：

**一、问题总览** — 表格列出所有请求的核心排查信号

```
| # | 风险  | traceId        | 接口                     | 状态码 | 耗时   | 日志信号         |
|---|-------|----------------|--------------------------|--------|--------|------------------|
| 1 | ERROR | 9510ac35...    | /v1/pt/hospital/taskDetail | 500  | 1200ms | ERROR=1 WARN=2  |
```

**二、单请求排查卡片** — 每个请求独立成节，包含：

- 请求信息（URL、方法、状态码、耗时、时间戳、关联服务）
- 初步判断（风险等级、主要证据、建议排查方向）
- 关键日志（按时间排序，优先 ERROR/WARN/异常栈）
- 相关 SQL 与 EXPLAIN 执行计划（一对一关联）
- 涉及表的数据量和索引信息

**三、原始证据归档** — 各服务完整日志文件，用于审计和二次分析

---

## 构建与发布

### macOS 本地构建

```sh
# 收集端
cd collector/src-tauri && cargo tauri build

# 分析端
cd analyzer/src-tauri && cargo tauri build
```

### Windows EXE 打包（在 Windows 机器上执行）

```powershell
# 安装依赖（一次性）
cargo install tauri-cli --version "^2"

# 打包收集端
cd collector
cargo tauri build --target x86_64-pc-windows-msvc
```

输出位于：

```
collector/src-tauri/target/x86_64-pc-windows-msvc/release/bundle/
├── msi/     # Smart Diag Collector_0.1.0_x64_en-US.msi
└── nsis/    # Smart Diag Collector_0.1.0_x64-setup.exe
```

> **注意**：Tauri 的 Windows 打包器（WiX/NSIS）只能在 Windows 上运行。如需在 CI 中自动化，请使用 GitHub Actions 的 `windows-latest` Runner。

---

## 项目结构

```
Smart-Diagnostic-System/
├── Cargo.toml                         # Workspace 定义
├── crates/
│   └── diag-core/                     # 共享库
│       └── src/
│           ├── config.rs              # 配置结构体（CollectorConfig 等）
│           ├── models.rs              # 数据模型（DiagnosisManifest、LogEntry 等）
│           ├── log_parser.rs          # 日志解析（JSON / 纯文本格式）
│           ├── sql_parser.rs          # SQL 提取（MyBatis / Hibernate / 通用模式）
│           ├── masking.rs             # 隐私脱敏
│           ├── url_resolver.rs        # URL → 服务名解析
│           ├── package.rs             # ZIP 打包逻辑
│           └── collector_trait.rs     # LogCollector / ServiceDiscovery Trait
├── collector/
│   ├── collector.toml.example         # 配置文件示例
│   ├── src/                           # 前端（纯 HTML/JS/CSS，无构建步骤）
│   └── src-tauri/
│       └── src/
│           ├── commands.rs            # Tauri 命令入口（AppState）
│           ├── diagnosis.rs           # DiagnosisRunner 采集编排器
│           ├── webview_capture.rs     # 诊断浏览器 + JS 注入
│           ├── elk_collector.rs       # ELK 日志采集后端
│           ├── ssh_log_collector.rs   # SSH 日志采集后端
│           ├── db_collector.rs        # 慢 SQL 查询
│           ├── explain_collector.rs   # EXPLAIN 执行计划采集
│           ├── sql_extractor.rs       # 从日志提取 SQL
│           ├── scheduler.rs           # 定时巡检调度器
│           ├── deployment.rs          # CSV 解析 + DeploymentManifest
│           ├── validator.rs           # SSH / DB / ELK 连通性校验
│           ├── config_store.rs        # 配置持久化
│           ├── dedup_cache.rs         # traceId 去重缓存
│           ├── nacos_discovery.rs     # Nacos 服务发现（预留扩展）
│           └── cleanup.rs             # 旧诊断包定期清理
└── analyzer/
    ├── src/                           # 前端（纯 HTML/JS/CSS）
    └── src-tauri/
        └── src/
            ├── commands.rs            # Tauri 命令入口
            ├── rule_engine.rs         # 规则引擎（风险判断逻辑）
            └── report.rs              # Markdown 报告生成
```

---

## 关键约定

| 约定 | 说明 |
|---|---|
| **用户界面语言** | 所有用户可见字符串均为中文；代码注释中英混用 |
| **JSON 字段命名** | 所有 `serde` 共享类型使用 `camelCase` |
| **自定义协议** | `diag://collect` 接收浏览器捕获数据，`diag://count` 接收实时请求计数 |
| **TLS 证书** | ELK 和 HTTP 客户端接受自签名证书（医院内网常见情况） |
| **SSH 主机校验** | 当前 MVP 跳过主机密钥校验（接受所有密钥） |
| **ELK 兼容性** | 自动探测 ES 直连或 Kibana 代理模式，兼容 ES 6.x / 7.x / 8.x |
| **数据库支持** | MySQL（`performance_schema`）和 PostgreSQL（`pg_stat_statements`）|
| **PostgreSQL 表名** | 报告中展示为 `schema.table`，避免跨 Schema 同名表误判 |
| **输出路径格式** | `{output_dir}/diagnosis-YYYYMMDD-HHMMSS-<page-slug>.zip` |
| **配置自动保存** | 部署配置保存至 `app_data_dir/deployment-config.json`，重启自动加载 |

---

## 隐私与安全

- URL 查询参数默认全部脱敏为 `***`，仅保留白名单参数（`pageNum`、`pageSize` 等）
- 诊断包不含完整业务数据，仅保留排查所需的日志片段和 SQL 统计摘要
- SSH 私钥内容不写入诊断包
- 数据库密码明文存储在配置文件中，建议限制配置文件的文件系统访问权限

---

## 许可证

MIT © vpcoderli
# Smart-Diagnostic-System
