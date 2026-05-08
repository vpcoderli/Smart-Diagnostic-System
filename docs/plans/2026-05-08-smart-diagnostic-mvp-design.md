# Smart Diagnostic System — MVP 设计方案

> 日期：2026-05-08 | 状态：设计确认 | 技术栈：Rust + Tauri | 团队：1人

---

## 1. 项目定位

面向 toB 医疗项目的**线上问题诊断闭环系统**，解决"现场运维拿不到有效信息、研发无法进入医院内网排查"的核心痛点。

## 2. 双端架构

| 端 | 应用名 | 运行环境 | 职责 |
|---|---|---|---|
| 收集端 | `smart-diag-collector` | 医院内网 Windows 跳板机 | 抓包、采集日志/SQL、脱敏打包 |
| 分析端 | `smart-diag-analyzer` | 研发侧 Windows/Mac | 导入诊断包、分析、生成报告 |

## 3. 项目结构（Rust Workspace）

```
smart-diagnostic-system/
├── Cargo.toml                 # Workspace root
├── crates/
│   └── diag-core/             # 共享核心库
│       ├── models/            # 诊断包数据结构
│       ├── url_resolver/      # URL → 服务解析
│       ├── log_parser/        # 日志解析器
│       ├── sql_parser/        # 慢 SQL 解析器
│       ├── masking/           # 隐私脱敏
│       └── package/           # zip 打包/解包
├── collector/                 # 收集端 Tauri 应用
│   ├── src-tauri/
│   └── src/                   # 前端 UI
└── analyzer/                  # 分析端 Tauri 应用
    ├── src-tauri/
    └── src/                   # 前端 UI
```

---

## 4. 收集端设计

### 4.1 运维操作流程（只需3步）

```
1. 打开收集端 → 输入页面 URL
2. 工具内嵌浏览器打开页面（如需登录先登录），运维操作/复现问题
3. 点击「采集完成」→ 自动汇总请求 → SSH采集日志 → 查询慢SQL → 脱敏打包 → 下载 diagnosis.zip
```

### 4.2 内嵌 WebView 抓包

Tauri 在 Windows 上使用 WebView2，打开独立窗口加载目标页面，注入诊断 JS 拦截所有 fetch/XHR。

**注入的诊断 JS：**

```javascript
const _fetch = window.fetch;
window.__diag_requests = [];

window.fetch = async (url, opts) => {
  const start = performance.now();
  const resp = await _fetch(url, opts);
  window.__diag_requests.push({
    method: opts?.method || 'GET',
    url: typeof url === 'string' ? url : url.toString(),
    status: resp.status,
    durationMs: Math.round(performance.now() - start),
    traceId: resp.headers.get('x-trace'),
    timestamp: new Date().toISOString()
  });
  return resp;
};

// XMLHttpRequest 拦截
const _XHROpen = XMLHttpRequest.prototype.open;
const _XHRSend = XMLHttpRequest.prototype.send;
XMLHttpRequest.prototype.open = function(method, url) {
  this.__diag = { method, url, start: 0 };
  _XHROpen.apply(this, arguments);
};
XMLHttpRequest.prototype.send = function() {
  this.__diag.start = performance.now();
  this.addEventListener('load', () => {
    window.__diag_requests.push({
      method: this.__diag.method,
      url: this.__diag.url,
      status: this.status,
      durationMs: Math.round(performance.now() - this.__diag.start),
      traceId: this.getResponseHeader('x-trace'),
      timestamp: new Date().toISOString()
    });
  });
  _XHRSend.apply(this, arguments);
};

window.__getDiagData = () => JSON.stringify({
  pageUrl: location.href,
  requests: window.__diag_requests
});
```

**Rust 侧获取数据：**

```rust
let diag_json = webview.eval("window.__getDiagData()")?;
let captured: CapturedPage = serde_json::from_str(&diag_json)?;

for req in &captured.requests {
    let resolved = resolve_url(&req.url, &config.gateway.prefix);
    // → service = "pcm-management"
    // → 查配置找到 hosts = ["172.29.60.10", "172.29.60.11"]
    // → SSH 采集日志 + DB 查询慢 SQL
}
```

### 4.3 三种远程能力

| 能力 | 用途 | Rust Crate |
|------|------|------------|
| HTTP (WebView) | 页面加载 + 请求拦截 | Tauri WebView2 |
| SSH | 远程登录 Linux 服务器 grep 日志 | `russh` |
| DB Client | 连接 MySQL/PostgreSQL 查询慢 SQL | `sqlx` |

### 4.4 SSH 日志采集

```rust
async fn collect_logs(
    ssh: &SshConfig,
    service: &ServiceConfig,
    trace_id: &str,
) -> Result<Vec<String>> {
    let mut all_logs = vec![];
    for host in &service.hosts {
        let session = ssh_connect(host, ssh).await?;
        let cmd = format!(
            "grep '{}' {}{}",
            trace_id, service.log_dir, service.log_pattern
        );
        let output = session.exec(&cmd).await?;
        all_logs.extend(parse_log_lines(&output));
    }
    Ok(all_logs)
}
```

### 4.5 DB 慢 SQL 查询

```rust
// MySQL
async fn collect_mysql_slow_sql(pool: &MySqlPool) -> Result<Vec<SlowSqlItem>> {
    sqlx::query_as!(SlowSqlItem,
        r#"SELECT digest_text as sql_fingerprint,
                  avg_timer_wait/1000000000 as duration_ms,
                  sum_rows_examined as rows_examined,
                  sum_rows_sent as rows_returned
           FROM performance_schema.events_statements_summary_by_digest
           WHERE avg_timer_wait > 1000000000
           ORDER BY avg_timer_wait DESC LIMIT 20"#
    ).fetch_all(pool).await
}

// PostgreSQL
async fn collect_pg_slow_sql(pool: &PgPool) -> Result<Vec<SlowSqlItem>> {
    sqlx::query_as!(SlowSqlItem,
        r#"SELECT query as sql_fingerprint,
                  mean_exec_time as duration_ms,
                  rows as rows_returned
           FROM pg_stat_statements
           WHERE mean_exec_time > 1000
           ORDER BY mean_exec_time DESC LIMIT 20"#
    ).fetch_all(pool).await
}
```

### 4.6 配置文件（collector.toml）

```toml
[site]
name = "hospital-a"
system = "pcm"

[gateway]
prefix = "/gateway"

[[services]]
name = "pcm-management"
display = "业务管理服务"
hosts = ["172.29.60.10", "172.29.60.11"]
log_dir = "/var/log/pcm-management/"
log_pattern = "*.log"

[[services]]
name = "pcm-followup"
display = "随访服务"
hosts = ["172.29.60.12"]
log_dir = "/var/log/pcm-followup/"
log_pattern = "*.log"

[[services]]
name = "pcm-server"
display = "患者管理服务"
hosts = ["172.29.60.13"]
log_dir = "/var/log/pcm-server/"
log_pattern = "*.log"

[[services]]
name = "pcm-communication"
display = "会话服务"
hosts = ["172.29.60.14"]
log_dir = "/var/log/pcm-communication/"
log_pattern = "*.log"

[[services]]
name = "pcm-profile"
display = "画像服务"
hosts = ["172.29.60.15"]
log_dir = "/var/log/pcm-profile/"
log_pattern = "*.log"

[[services]]
name = "pcm-data"
display = "数据服务"
hosts = ["172.29.60.16"]
log_dir = "/var/log/pcm-data/"
log_pattern = "*.log"

[[services]]
name = "pcm-statistics"
display = "数据分析服务"
hosts = ["172.29.60.17"]
log_dir = "/var/log/pcm-statistics/"
log_pattern = "*.log"

[[services]]
name = "pcm-user"
display = "用户服务"
hosts = ["172.29.60.18"]
log_dir = "/var/log/pcm-user/"
log_pattern = "*.log"

[[services]]
name = "pcm-channel"
display = "通道服务"
hosts = ["172.29.60.19"]
log_dir = "/var/log/pcm-channel/"
log_pattern = "*.log"

[[services]]
name = "pcm-health-plan"
display = "健康方案服务"
hosts = ["172.29.60.20"]
log_dir = "/var/log/pcm-health-plan/"
log_pattern = "*.log"

[[services]]
name = "pcm-open-api"
display = "外部接口服务"
hosts = ["172.29.60.21"]
log_dir = "/var/log/pcm-open-api/"
log_pattern = "*.log"

[ssh]
port = 22
username = "ops"
auth_type = "key"
private_key = "C:\\Users\\ops\\.ssh\\id_rsa"

[database]
type = "mysql"
host = "172.29.60.100"
port = 3306
username = "readonly"
password = ""
database = "pcm_management"

[privacy]
mask_query_values = true
allowed_query_keys = ["pageNum", "pageSize", "portal"]

[collector]
time_window_minutes = 5
max_log_lines = 5000
output_dir = "C:\\diagnosis-output\\"
```

---

## 5. 诊断包格式（diagnosis.zip）

```
diagnosis-hospital-a-20260508-143000.zip
├── manifest.json
├── browser/
│   ├── page.json           # 页面信息
│   └── requests.json       # 所有捕获的 API 请求
├── services/
│   ├── pcm-management/
│   │   ├── app-log.jsonl    # traceId 关联的日志
│   │   └── error-stack.txt  # 异常堆栈
│   └── pcm-user/
│       └── app-log.jsonl
├── database/
│   ├── slow-sql.json        # 慢 SQL 列表
│   └── table-stats.json     # 表行数/索引信息
└── privacy/
    └── masking-report.json  # 脱敏记录
```

**manifest.json 示例：**

```json
{
  "diagnosisId": "diag-20260508-143000",
  "site": "hospital-a",
  "system": "pcm",
  "createdAt": "2026-05-08T14:30:00+08:00",
  "pageUrl": "http://172.29.60.151/patient-management",
  "requestCount": 12,
  "services": ["pcm-management", "pcm-user"],
  "traceIds": ["abc123", "def456", "ghi789"],
  "databaseType": "mysql",
  "privacyLevel": "MASKED",
  "collectorVersion": "0.1.0"
}
```

---

## 6. 分析端设计

### 6.1 四个核心视图

1. **导入面板** — 拖入 diagnosis.zip，展示诊断包基本信息
2. **证据仪表盘** — 请求概览 + 日志摘要 + SQL 风险卡片
3. **诊断结论** — 规则引擎输出根因候选 + 证据链 + 建议
4. **报告导出** — 一键生成 Markdown 诊断报告

### 6.2 规则引擎（MVP 硬编码）

```rust
fn diagnose(evidence: &Evidence) -> Vec<Finding> {
    let mut findings = vec![];

    // 规则1: SQL 耗时占比高
    for req in &evidence.requests {
        if let Some(sql) = evidence.slowest_sql_for_trace(&req.trace_id) {
            let ratio = sql.duration_ms as f64 / req.duration_ms as f64;
            if ratio > 0.6 && sql.duration_ms > 1000 {
                findings.push(Finding::slow_sql(req, sql, ratio));
            }
        }
    }

    // 规则2: 后端异常
    for log in &evidence.error_logs {
        findings.push(Finding::backend_exception(log));
    }

    // 规则3: 慢接口无 SQL 关联
    for req in &evidence.requests {
        if req.duration_ms > 2000
            && evidence.slowest_sql_for_trace(&req.trace_id).is_none() {
            findings.push(Finding::slow_api_no_sql(req));
        }
    }

    findings
}
```

---

## 7. 关键 Rust 依赖

| Crate | 用途 |
|-------|------|
| `tauri` 2.x | 桌面应用框架 |
| `russh` | SSH 远程连接 |
| `sqlx` | MySQL / PostgreSQL 客户端 |
| `serde` + `serde_json` | 序列化 |
| `toml` | 配置文件解析 |
| `zip` | 诊断包打包/解包 |
| `chrono` | 时间处理 |
| `regex` | 日志解析 |
| `tokio` | 异步运行时 |

---

## 8. 实施计划（一人、6周）

| 周 | 目标 | 交付物 |
|----|------|--------|
| W1 | 项目脚手架 + diag-core 基础 | Workspace 结构、数据模型、URL 解析器、配置解析 |
| W2 | 收集端：WebView 抓包 | Tauri 双窗口、JS 注入、请求捕获、数据回传 |
| W3 | 收集端：SSH + DB | SSH 日志采集、MySQL/PostgreSQL 慢 SQL 查询 |
| W4 | 收集端：脱敏 + 打包 | Privacy Masker、diagnosis.zip 生成、收集端 UI |
| W5 | 分析端：导入 + 分析 | 诊断包解析、证据仪表盘、规则引擎 |
| W6 | 分析端：报告 + 联调 | 报告生成、端到端测试、试点准备 |

---

## 9. MVP 验收标准

1. 运维输入页面 URL，内嵌浏览器能加载页面并捕获所有 API 请求
2. 能自动识别每个请求对应的 pcm-* 服务
3. 能 SSH 到对应 Linux 服务器按 traceId grep 日志
4. 能连接 MySQL/PostgreSQL 查询慢 SQL
5. 能脱敏并生成标准 diagnosis.zip
6. 分析端能导入诊断包并展示请求/日志/SQL 证据
7. 规则引擎能输出根因候选和短中长期建议
8. 能生成 Markdown 诊断报告
