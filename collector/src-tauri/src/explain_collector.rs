use diag_core::config::DatabaseConfig;
use diag_core::models::{ExplainPlan, ExplainRow, SlowSqlItem, SqlTrace};
use serde_json::Value as JsonValue;

const MAX_EXPLAIN_COUNT: usize = 20;

fn should_explain(avg_duration_ms: f64, threshold_ms: f64) -> bool {
    avg_duration_ms > threshold_ms
}

pub struct ExplainCollector {
    config: DatabaseConfig,
    threshold_ms: f64,
}

impl ExplainCollector {
    pub fn new(config: DatabaseConfig, threshold_ms: f64) -> Self {
        Self {
            config,
            threshold_ms,
        }
    }

    /// 针对 db_collector 来源的慢 SQL（已是规范化文本）执行 EXPLAIN
    pub async fn collect_explain_plans(&self, slow_sqls: &[SlowSqlItem]) -> Vec<ExplainPlan> {
        let candidates: Vec<&SlowSqlItem> = slow_sqls
            .iter()
            .filter(|s| should_explain(s.duration_ms, self.threshold_ms))
            .take(MAX_EXPLAIN_COUNT)
            .collect();

        if candidates.is_empty() {
            return Vec::new();
        }

        match self.config.db_type.as_str() {
            "mysql" => self.explain_mysql_slow(&candidates).await,
            "postgresql" | "postgres" => self.explain_postgresql_slow(&candidates).await,
            _ => {
                tracing::warn!("EXPLAIN 不支持数据库类型: {}", self.config.db_type);
                Vec::new()
            }
        }
    }

    /// 针对从日志提取的 SqlTrace 执行 EXPLAIN（参数拼装后再跑）
    pub async fn collect_explain_for_sql_traces(
        &self,
        sql_traces: &[SqlTrace],
    ) -> Vec<ExplainPlan> {
        // 按 fingerprint 去重，但保留首个 trace 的 traceId 与 sql/params
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut candidates: Vec<&SqlTrace> = Vec::new();
        for t in sql_traces {
            if seen.insert(t.sql_fingerprint.clone()) {
                candidates.push(t);
                if candidates.len() >= MAX_EXPLAIN_COUNT {
                    break;
                }
            }
        }

        if candidates.is_empty() {
            return Vec::new();
        }

        match self.config.db_type.as_str() {
            "mysql" => self.explain_mysql_traces(&candidates).await,
            "postgresql" | "postgres" => self.explain_postgresql_traces(&candidates).await,
            _ => {
                tracing::warn!("EXPLAIN 不支持数据库类型: {}", self.config.db_type);
                Vec::new()
            }
        }
    }

    async fn explain_mysql_slow(&self, sqls: &[&SlowSqlItem]) -> Vec<ExplainPlan> {
        let url = self.mysql_url();
        let pool = match sqlx::MySqlPool::connect(&url).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("EXPLAIN MySQL 连接失败: {}", e);
                return Vec::new();
            }
        };

        let mut plans = Vec::new();
        for sql_item in sqls {
            let plan = run_mysql_explain(
                &pool,
                &sql_item.sql_fingerprint,
                sql_item.duration_ms,
                "mysql_explain",
                None,
                None,
            )
            .await;
            plans.push(plan);
        }
        pool.close().await;
        plans
    }

    async fn explain_mysql_traces(&self, traces: &[&SqlTrace]) -> Vec<ExplainPlan> {
        let url = self.mysql_url();
        let pool = match sqlx::MySqlPool::connect(&url).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("EXPLAIN MySQL 连接失败: {}", e);
                return Vec::new();
            }
        };

        let mut plans = Vec::new();
        for t in traces {
            let executed_sql = match prepare_explain_sql(&t.sql, t.parameters.as_deref(), "mysql") {
                Ok(sql) => sql,
                Err(e) => {
                    plans.push(ExplainPlan {
                        sql_fingerprint: t.sql_fingerprint.clone(),
                        avg_duration_ms: t.duration_ms.unwrap_or(0.0),
                        source: "log_sql_explain".into(),
                        explain_rows: Vec::new(),
                        table_stats: None,
                        trace_id: Some(t.trace_id.clone()),
                        executed_sql: Some(build_executable_sql(&t.sql, t.parameters.as_deref())),
                        error: Some(e),
                        found_in_schema: None,
                    });
                    continue;
                }
            };
            let plan = run_mysql_explain(
                &pool,
                &executed_sql,
                t.duration_ms.unwrap_or(0.0),
                "log_sql_explain",
                Some(t.trace_id.clone()),
                Some(executed_sql.clone()),
            )
            .await;
            plans.push(ExplainPlan {
                sql_fingerprint: t.sql_fingerprint.clone(),
                found_in_schema: None, // MySQL 不区分 schema
                ..plan
            });
        }
        pool.close().await;
        plans
    }

    async fn explain_postgresql_slow(&self, sqls: &[&SlowSqlItem]) -> Vec<ExplainPlan> {
        let url = self.pg_url();
        let pool = match sqlx::PgPool::connect(&url).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("EXPLAIN PostgreSQL 连接失败: {}", e);
                return Vec::new();
            }
        };

        let mut plans = Vec::new();
        for sql_item in sqls {
            // 跳过含 $? 占位符的系统查询（pg_stat_statements 原始 SQL）
            if sql_item.sql_fingerprint.contains("$?") {
                tracing::info!(
                    "跳过含 $? 的系统查询: {}...",
                    &sql_item.sql_fingerprint[..sql_item.sql_fingerprint.len().min(60)]
                );
                continue;
            }

            let mut found_plan: Option<ExplainPlan> = None;
            let mut last_error: Option<String> = None;
            let candidate_schemas = self
                .resolve_pg_candidate_schemas(&pool, &sql_item.tables)
                .await;
            if !candidate_schemas.is_empty() {
                for schema in &candidate_schemas {
                    // 使用专用连接保证 SET 和 EXPLAIN 在同一连接上执行
                    let mut conn = match pool.acquire().await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!("获取连接失败: {}", e);
                            continue;
                        }
                    };
                    let search_path =
                        format!("SET search_path TO {}, public", quote_pg_ident(schema));
                    if let Err(e) = sqlx::query(&search_path).execute(&mut *conn).await {
                        tracing::warn!("SET search_path TO {} 失败: {}", schema, e);
                        continue;
                    }

                    let plan = run_pg_explain_on_conn(
                        &mut conn,
                        &sql_item.sql_fingerprint,
                        sql_item.duration_ms,
                        "pg_explain",
                        None,
                        None,
                    )
                    .await;

                    if plan.error.is_none() {
                        tracing::info!(
                            "EXPLAIN 成功 (schema={}): {}...",
                            schema,
                            &sql_item.sql_fingerprint[..sql_item.sql_fingerprint.len().min(40)]
                        );
                        found_plan = Some(ExplainPlan {
                            found_in_schema: Some(schema.clone()),
                            ..plan
                        });
                        break;
                    } else {
                        last_error = plan.error.clone();
                    }
                }
            } else {
                let mut conn = match pool.acquire().await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("获取连接失败: {}", e);
                        continue;
                    }
                };
                let plan = run_pg_explain_on_conn(
                    &mut conn,
                    &sql_item.sql_fingerprint,
                    sql_item.duration_ms,
                    "pg_explain",
                    None,
                    None,
                )
                .await;
                found_plan = Some(plan);
            }

            if let Some(p) = found_plan {
                plans.push(p);
            } else {
                plans.push(ExplainPlan {
                    sql_fingerprint: sql_item.sql_fingerprint.clone(),
                    avg_duration_ms: sql_item.duration_ms,
                    source: "pg_explain".into(),
                    explain_rows: Vec::new(),
                    table_stats: None,
                    trace_id: None,
                    executed_sql: None,
                    error: Some(
                        last_error
                            .map(|e| format!("所有候选 schema 均无法执行 EXPLAIN；最后错误: {}", e))
                            .unwrap_or_else(|| "所有候选 schema 均无法执行 EXPLAIN".into()),
                    ),
                    found_in_schema: None,
                });
            }
        }
        pool.close().await;
        plans
    }

    async fn explain_postgresql_traces(&self, traces: &[&SqlTrace]) -> Vec<ExplainPlan> {
        let url = self.pg_url();
        let pool = match sqlx::PgPool::connect(&url).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("EXPLAIN PostgreSQL 连接失败: {}", e);
                return Vec::new();
            }
        };

        let mut plans = Vec::new();
        for t in traces {
            // 跳过含 $? 占位符的系统查询
            if t.sql.contains("$?") || t.sql_fingerprint.contains("$?") {
                tracing::info!("跳过含 $? 的系统查询 (trace={})", t.trace_id);
                continue;
            }

            let executed_sql =
                match prepare_explain_sql(&t.sql, t.parameters.as_deref(), "postgresql") {
                    Ok(sql) => sql,
                    Err(e) => {
                        plans.push(ExplainPlan {
                            sql_fingerprint: t.sql_fingerprint.clone(),
                            avg_duration_ms: t.duration_ms.unwrap_or(0.0),
                            source: "log_sql_explain".into(),
                            explain_rows: Vec::new(),
                            table_stats: None,
                            trace_id: Some(t.trace_id.clone()),
                            executed_sql: Some(build_executable_sql(
                                &t.sql,
                                t.parameters.as_deref(),
                            )),
                            error: Some(e),
                            found_in_schema: None,
                        });
                        continue;
                    }
                };

            // 多 schema 场景：逐个尝试，每次用专用连接保证 SET 和 EXPLAIN 同一连接执行
            let mut found_plan: Option<ExplainPlan> = None;
            let mut last_error: Option<String> = None;
            let candidate_schemas = self.resolve_pg_candidate_schemas(&pool, &t.tables).await;
            if !candidate_schemas.is_empty() {
                for schema in &candidate_schemas {
                    let mut conn = match pool.acquire().await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!("获取连接失败: {}", e);
                            continue;
                        }
                    };
                    let search_path =
                        format!("SET search_path TO {}, public", quote_pg_ident(schema));
                    if let Err(e) = sqlx::query(&search_path).execute(&mut *conn).await {
                        tracing::warn!("SET search_path TO {} 失败: {}", schema, e);
                        continue;
                    }

                    let plan = run_pg_explain_on_conn(
                        &mut conn,
                        &executed_sql,
                        t.duration_ms.unwrap_or(0.0),
                        "log_sql_explain",
                        Some(t.trace_id.clone()),
                        Some(executed_sql.clone()),
                    )
                    .await;

                    if plan.error.is_none() {
                        tracing::info!("EXPLAIN 成功 (schema={}, trace={})", schema, t.trace_id);
                        found_plan = Some(ExplainPlan {
                            sql_fingerprint: t.sql_fingerprint.clone(),
                            found_in_schema: Some(schema.clone()),
                            ..plan
                        });
                        break;
                    } else {
                        last_error = plan.error.clone();
                    }
                }
            } else {
                // 没配置 schemas → 直接执行
                let mut conn = match pool.acquire().await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("获取连接失败: {}", e);
                        continue;
                    }
                };
                let plan = run_pg_explain_on_conn(
                    &mut conn,
                    &executed_sql,
                    t.duration_ms.unwrap_or(0.0),
                    "log_sql_explain",
                    Some(t.trace_id.clone()),
                    Some(executed_sql.clone()),
                )
                .await;
                found_plan = Some(ExplainPlan {
                    sql_fingerprint: t.sql_fingerprint.clone(),
                    ..plan
                });
            }

            if let Some(p) = found_plan {
                plans.push(p);
            } else {
                plans.push(ExplainPlan {
                    sql_fingerprint: t.sql_fingerprint.clone(),
                    avg_duration_ms: t.duration_ms.unwrap_or(0.0),
                    source: "log_sql_explain".into(),
                    explain_rows: Vec::new(),
                    table_stats: None,
                    trace_id: Some(t.trace_id.clone()),
                    executed_sql: Some(executed_sql.clone()),
                    error: Some(
                        last_error
                            .map(|e| format!("所有候选 schema 均无法执行 EXPLAIN；最后错误: {}", e))
                            .unwrap_or_else(|| "所有候选 schema 均无法执行 EXPLAIN".into()),
                    ),
                    found_in_schema: None,
                });
            }
        }
        pool.close().await;
        plans
    }

    fn mysql_url(&self) -> String {
        format!(
            "mysql://{}:{}@{}:{}/{}",
            self.config.username,
            self.config.password,
            self.config.host,
            self.config.port,
            self.config.database
        )
    }

    fn pg_url(&self) -> String {
        format!(
            "postgres://{}:{}@{}:{}/{}",
            self.config.username,
            self.config.password,
            self.config.host,
            self.config.port,
            self.config.database
        )
    }

    async fn resolve_pg_candidate_schemas(
        &self,
        pool: &sqlx::PgPool,
        tables: &[String],
    ) -> Vec<String> {
        let discovered = match discover_pg_table_schemas(pool, tables).await {
            Ok(schemas) => schemas,
            Err(e) => {
                tracing::warn!("查询 SQL 涉及表所在 schema 失败: {}", e);
                Vec::new()
            }
        };
        merge_schema_candidates(&self.config.schemas, discovered)
    }
}

/// 拼装可执行 SQL：有参数则替换 ?，否则原样返回
fn build_executable_sql(sql: &str, params: Option<&str>) -> String {
    match params {
        Some(p) if !p.trim().is_empty() => crate::sql_extractor::substitute_parameters(sql, p),
        _ => sql.to_string(),
    }
}

fn prepare_explain_sql(sql: &str, params: Option<&str>, db_type: &str) -> Result<String, String> {
    let mut executed = build_executable_sql(sql, params);
    if has_unresolved_placeholder(&executed) {
        return Err(
            "SQL 参数未完整拼装，仍包含 ? 占位符；请检查 Parameters 日志是否被采集到".into(),
        );
    }
    if matches!(db_type, "postgresql" | "postgres") {
        executed = convert_mysql_backticks_for_postgres(&executed);
    }
    Ok(executed)
}

fn has_unresolved_placeholder(sql: &str) -> bool {
    let mut in_quote = false;
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\'' {
            if in_quote && i + 1 < chars.len() && chars[i + 1] == '\'' {
                i += 2;
                continue;
            }
            in_quote = !in_quote;
        } else if ch == '?' && !in_quote {
            return true;
        }
        i += 1;
    }
    false
}

fn convert_mysql_backticks_for_postgres(sql: &str) -> String {
    sql.replace('`', "\"")
}

async fn discover_pg_table_schemas(
    pool: &sqlx::PgPool,
    tables: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    if tables.is_empty() {
        return Ok(Vec::new());
    }

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT table_schema \
         FROM information_schema.tables \
         WHERE table_name = ANY($1) \
           AND table_type = 'BASE TABLE' \
           AND table_schema NOT IN ('pg_catalog', 'information_schema', 'pg_toast') \
         ORDER BY table_schema",
    )
    .bind(tables)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|(schema,)| schema).collect())
}

fn merge_schema_candidates(configured: &[String], discovered: Vec<String>) -> Vec<String> {
    let mut merged = Vec::new();
    for schema in configured.iter().chain(discovered.iter()) {
        if !schema.is_empty() && !merged.contains(schema) {
            merged.push(schema.clone());
        }
    }
    merged
}

async fn run_mysql_explain(
    pool: &sqlx::MySqlPool,
    sql: &str,
    avg_duration_ms: f64,
    source: &str,
    trace_id: Option<String>,
    executed_sql: Option<String>,
) -> ExplainPlan {
    let explain_sql = format!("EXPLAIN {}", sql);
    let fingerprint = diag_core::sql_parser::fingerprint_sql(sql);

    match sqlx::query_as::<_, MysqlExplainRow>(&explain_sql)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => {
            let explain_rows: Vec<ExplainRow> = rows
                .into_iter()
                .map(|r| ExplainRow {
                    id: r.id,
                    select_type: r.select_type,
                    table: r.table_name,
                    access_type: r.type_field,
                    possible_keys: r.possible_keys,
                    key: r.key_name,
                    rows: r.rows,
                    filtered: r.filtered,
                    extra: r.extra,
                })
                .collect();

            ExplainPlan {
                sql_fingerprint: fingerprint,
                avg_duration_ms,
                source: source.to_string(),
                explain_rows,
                table_stats: None,
                trace_id,
                executed_sql,
                error: None,
                found_in_schema: None,
            }
        }
        Err(e) => {
            tracing::warn!("EXPLAIN 执行失败 ({}): {}", sql, e);
            ExplainPlan {
                sql_fingerprint: fingerprint,
                avg_duration_ms,
                source: source.to_string(),
                explain_rows: Vec::new(),
                table_stats: None,
                trace_id,
                executed_sql,
                error: Some(e.to_string()),
                found_in_schema: None,
            }
        }
    }
}

/// 在专用连接上执行 EXPLAIN（保证 SET search_path 和 EXPLAIN 在同一连接）
async fn run_pg_explain_on_conn(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    sql: &str,
    avg_duration_ms: f64,
    source: &str,
    trace_id: Option<String>,
    executed_sql: Option<String>,
) -> ExplainPlan {
    let explain_sql = format!("EXPLAIN (FORMAT JSON) {}", sql);
    let fingerprint = diag_core::sql_parser::fingerprint_sql(sql);

    match sqlx::query_scalar::<_, JsonValue>(&explain_sql)
        .fetch_one(&mut **conn)
        .await
    {
        Ok(json_val) => {
            let json_str =
                serde_json::to_string_pretty(&json_val).unwrap_or_else(|_| json_val.to_string());
            ExplainPlan {
                sql_fingerprint: fingerprint,
                avg_duration_ms,
                source: source.to_string(),
                explain_rows: vec![ExplainRow {
                    id: None,
                    select_type: None,
                    table: None,
                    access_type: None,
                    possible_keys: None,
                    key: None,
                    rows: None,
                    filtered: None,
                    extra: Some(json_str),
                }],
                table_stats: None,
                trace_id,
                executed_sql,
                error: None,
                found_in_schema: None,
            }
        }
        Err(e) => ExplainPlan {
            sql_fingerprint: fingerprint,
            avg_duration_ms,
            source: source.to_string(),
            explain_rows: Vec::new(),
            table_stats: None,
            trace_id,
            executed_sql,
            error: Some(e.to_string()),
            found_in_schema: None,
        },
    }
}

async fn run_pg_explain(
    pool: &sqlx::PgPool,
    sql: &str,
    avg_duration_ms: f64,
    source: &str,
    trace_id: Option<String>,
    executed_sql: Option<String>,
) -> ExplainPlan {
    let explain_sql = format!("EXPLAIN (FORMAT JSON) {}", sql);
    let fingerprint = diag_core::sql_parser::fingerprint_sql(sql);

    match sqlx::query_scalar::<_, JsonValue>(&explain_sql)
        .fetch_one(pool)
        .await
    {
        Ok(json_val) => {
            let json_str =
                serde_json::to_string_pretty(&json_val).unwrap_or_else(|_| json_val.to_string());
            ExplainPlan {
                sql_fingerprint: fingerprint,
                avg_duration_ms,
                source: source.to_string(),
                explain_rows: vec![ExplainRow {
                    id: None,
                    select_type: None,
                    table: None,
                    access_type: None,
                    possible_keys: None,
                    key: None,
                    rows: None,
                    filtered: None,
                    extra: Some(json_str),
                }],
                table_stats: None,
                trace_id,
                executed_sql,
                error: None,
                found_in_schema: None,
            }
        }
        Err(e) => {
            tracing::warn!("EXPLAIN 执行失败 ({}): {}", sql, e);
            ExplainPlan {
                sql_fingerprint: fingerprint,
                avg_duration_ms,
                source: source.to_string(),
                explain_rows: Vec::new(),
                table_stats: None,
                trace_id,
                executed_sql,
                error: Some(e.to_string()),
                found_in_schema: None,
            }
        }
    }
}

#[derive(sqlx::FromRow)]
struct MysqlExplainRow {
    id: Option<i32>,
    select_type: Option<String>,
    #[sqlx(rename = "table")]
    table_name: Option<String>,
    #[sqlx(rename = "type")]
    type_field: Option<String>,
    possible_keys: Option<String>,
    #[sqlx(rename = "key")]
    key_name: Option<String>,
    rows: Option<i64>,
    filtered: Option<f64>,
    #[sqlx(rename = "Extra")]
    extra: Option<String>,
}

/// 为 PostgreSQL 连接设置 search_path（如果配置中指定了 schema）
/// 多个 schema 按用户选择的顺序拼接，并自动追加 public 作为兜底
async fn set_pg_search_path(pool: &sqlx::PgPool, config: &DatabaseConfig) -> Result<(), String> {
    if config.schemas.is_empty() {
        return Ok(());
    }
    let mut parts: Vec<String> = config.schemas.iter().map(|s| quote_pg_ident(s)).collect();
    if !config.schemas.iter().any(|s| s == "public") {
        parts.push("public".to_string());
    }
    let sql = format!("SET search_path TO {}", parts.join(", "));
    sqlx::query(&sql)
        .execute(pool)
        .await
        .map_err(|e| format!("SET search_path 失败: {}", e))?;
    tracing::info!("{}", sql);
    Ok(())
}

/// 安全引用 PG 标识符（schema 名）：
/// - 全部小写、仅含字母数字下划线、不以数字开头 → 不加引号
/// - 否则用双引号包裹，并把内部的 " 转义为 ""
fn quote_pg_ident(name: &str) -> String {
    let is_safe = !name.is_empty()
        && name
            .chars()
            .next()
            .map(|c| c.is_ascii_lowercase() || c == '_')
            .unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if is_safe {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}
