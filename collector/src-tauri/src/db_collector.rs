use anyhow::{anyhow, Result};
use diag_core::config::DatabaseConfig;
use diag_core::models::{ExplainSummary, SlowSqlItem, TableStats, IndexInfo};
use diag_core::sql_parser;

/// 数据库采集器
pub struct DbCollector {
    config: DatabaseConfig,
}

impl DbCollector {
    pub fn new(config: DatabaseConfig) -> Self {
        Self { config }
    }

    /// 采集慢 SQL + 表统计信息
    pub async fn collect(&self) -> Result<(Vec<SlowSqlItem>, Vec<TableStats>)> {
        match self.config.db_type.as_str() {
            "mysql" => self.collect_mysql().await,
            "postgresql" | "postgres" => self.collect_postgresql().await,
            other => Err(anyhow!("不支持的数据库类型: {}", other)),
        }
    }

    // ─── MySQL ───

    async fn collect_mysql(&self) -> Result<(Vec<SlowSqlItem>, Vec<TableStats>)> {
        let url = format!(
            "mysql://{}:{}@{}:{}/{}",
            self.config.username,
            self.config.password,
            self.config.host,
            self.config.port,
            self.config.database
        );

        let pool = sqlx::MySqlPool::connect(&url)
            .await
            .map_err(|e| anyhow!("MySQL 连接失败 ({}:{}): {}", self.config.host, self.config.port, e))?;

        tracing::info!("MySQL 连接成功: {}:{}", self.config.host, self.config.port);

        let slow_sqls = self.query_mysql_slow_sql(&pool).await?;
        let table_stats = self.query_mysql_table_stats(&pool).await?;

        pool.close().await;
        Ok((slow_sqls, table_stats))
    }

    async fn query_mysql_slow_sql(&self, pool: &sqlx::MySqlPool) -> Result<Vec<SlowSqlItem>> {
        // 从 performance_schema 获取慢 SQL 摘要
        let rows: Vec<MysqlSlowRow> = sqlx::query_as(
            r#"SELECT
                 DIGEST_TEXT as digest_text,
                 COUNT_STAR as count_star,
                 AVG_TIMER_WAIT / 1000000000 as avg_duration_ms,
                 SUM_ROWS_EXAMINED as sum_rows_examined,
                 SUM_ROWS_SENT as sum_rows_sent
               FROM performance_schema.events_statements_summary_by_digest
               WHERE DIGEST_TEXT IS NOT NULL
                 AND AVG_TIMER_WAIT > 500000000
               ORDER BY AVG_TIMER_WAIT DESC
               LIMIT 20"#,
        )
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow!("查询 performance_schema 失败: {}", e))?;

        tracing::info!("MySQL 慢 SQL 查询到 {} 条", rows.len());

        let items: Vec<SlowSqlItem> = rows
            .into_iter()
            .map(|row| {
                let fingerprint = sql_parser::fingerprint_sql(&row.digest_text);
                let tables = sql_parser::extract_tables(&row.digest_text);
                let operation = sql_parser::detect_operation(&row.digest_text).to_string();

                SlowSqlItem {
                    trace_id: None,
                    database_type: "mysql".to_string(),
                    service: None,
                    sql_fingerprint: fingerprint,
                    duration_ms: row.avg_duration_ms,
                    tables,
                    operation: Some(operation),
                    rows_examined: Some(row.sum_rows_examined),
                    rows_returned: Some(row.sum_rows_sent),
                    index_used: None,
                    explain_summary: None,
                }
            })
            .collect();

        Ok(items)
    }

    async fn query_mysql_table_stats(&self, pool: &sqlx::MySqlPool) -> Result<Vec<TableStats>> {
        let db = &self.config.database;
        let rows: Vec<MysqlTableRow> = sqlx::query_as(
            r#"SELECT
                 TABLE_SCHEMA as table_schema,
                 TABLE_NAME as table_name,
                 TABLE_ROWS as table_rows,
                 DATA_LENGTH as data_length,
                 INDEX_LENGTH as index_length
               FROM information_schema.tables
               WHERE TABLE_SCHEMA = ?
                 AND TABLE_TYPE = 'BASE TABLE'
               ORDER BY TABLE_ROWS DESC
               LIMIT 50"#,
        )
        .bind(db)
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow!("查询 table stats 失败: {}", e))?;

        let mut stats = Vec::new();
        for row in rows {
            // 查询索引信息
            let indexes = self
                .query_mysql_indexes(pool, &row.table_name)
                .await
                .unwrap_or_default();

            stats.push(TableStats {
                schema: row.table_schema,
                table_name: row.table_name,
                row_count: row.table_rows.unwrap_or(0),
                data_size_bytes: row.data_length,
                index_size_bytes: row.index_length,
                indexes,
            });
        }

        tracing::info!("MySQL 表统计采集到 {} 张表", stats.len());
        Ok(stats)
    }

    async fn query_mysql_indexes(
        &self,
        pool: &sqlx::MySqlPool,
        table_name: &str,
    ) -> Result<Vec<IndexInfo>> {
        let rows: Vec<MysqlIndexRow> = sqlx::query_as(
            r#"SELECT
                 INDEX_NAME as index_name,
                 COLUMN_NAME as column_name,
                 NON_UNIQUE as non_unique
               FROM information_schema.statistics
               WHERE TABLE_SCHEMA = ?
                 AND TABLE_NAME = ?
               ORDER BY INDEX_NAME, SEQ_IN_INDEX"#,
        )
        .bind(&self.config.database)
        .bind(table_name)
        .fetch_all(pool)
        .await?;

        // 按 index_name 分组
        let mut index_map: std::collections::HashMap<String, (Vec<String>, bool)> =
            std::collections::HashMap::new();

        for row in rows {
            let entry = index_map
                .entry(row.index_name.clone())
                .or_insert_with(|| (Vec::new(), row.non_unique == 0));
            entry.0.push(row.column_name);
        }

        Ok(index_map
            .into_iter()
            .map(|(name, (columns, unique))| IndexInfo {
                name,
                columns,
                unique,
            })
            .collect())
    }

    // ─── PostgreSQL ───

    async fn collect_postgresql(&self) -> Result<(Vec<SlowSqlItem>, Vec<TableStats>)> {
        let url = format!(
            "postgres://{}:{}@{}:{}/{}",
            self.config.username,
            self.config.password,
            self.config.host,
            self.config.port,
            self.config.database
        );

        let pool = sqlx::PgPool::connect(&url)
            .await
            .map_err(|e| anyhow!("PostgreSQL 连接失败 ({}:{}): {}", self.config.host, self.config.port, e))?;

        tracing::info!("PostgreSQL 连接成功: {}:{}", self.config.host, self.config.port);

        let slow_sqls = self.query_pg_slow_sql(&pool).await?;
        let table_stats = self.query_pg_table_stats(&pool).await?;

        pool.close().await;
        Ok((slow_sqls, table_stats))
    }

    async fn query_pg_slow_sql(&self, pool: &sqlx::PgPool) -> Result<Vec<SlowSqlItem>> {
        // 尝试从 pg_stat_statements 获取（需要扩展已安装）
        let rows: Vec<PgSlowRow> = sqlx::query_as(
            r#"SELECT
                 query,
                 calls,
                 mean_exec_time as mean_time_ms,
                 rows
               FROM pg_stat_statements
               WHERE mean_exec_time > 500
                 AND query NOT LIKE '%pg_stat%'
               ORDER BY mean_exec_time DESC
               LIMIT 20"#,
        )
        .fetch_all(pool)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("pg_stat_statements 查询失败（可能未安装扩展）: {}", e);
            Vec::new()
        });

        tracing::info!("PostgreSQL 慢 SQL 查询到 {} 条", rows.len());

        let items: Vec<SlowSqlItem> = rows
            .into_iter()
            .map(|row| {
                let fingerprint = sql_parser::fingerprint_sql(&row.query);
                let tables = sql_parser::extract_tables(&row.query);
                let operation = sql_parser::detect_operation(&row.query).to_string();

                SlowSqlItem {
                    trace_id: None,
                    database_type: "postgresql".to_string(),
                    service: None,
                    sql_fingerprint: fingerprint,
                    duration_ms: row.mean_time_ms,
                    tables,
                    operation: Some(operation),
                    rows_examined: None,
                    rows_returned: Some(row.rows),
                    index_used: None,
                    explain_summary: None,
                }
            })
            .collect();

        Ok(items)
    }

    async fn query_pg_table_stats(&self, pool: &sqlx::PgPool) -> Result<Vec<TableStats>> {
        let rows: Vec<PgTableRow> = sqlx::query_as(
            r#"SELECT
                 schemaname,
                 relname as table_name,
                 n_live_tup as row_count,
                 pg_total_relation_size(relid) as total_size
               FROM pg_stat_user_tables
               ORDER BY n_live_tup DESC
               LIMIT 50"#,
        )
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow!("查询 pg_stat_user_tables 失败: {}", e))?;

        let mut stats = Vec::new();
        for row in rows {
            let indexes = self
                .query_pg_indexes(pool, &row.table_name)
                .await
                .unwrap_or_default();

            stats.push(TableStats {
                schema: row.schemaname,
                table_name: row.table_name,
                row_count: row.row_count,
                data_size_bytes: Some(row.total_size),
                index_size_bytes: None,
                indexes,
            });
        }

        tracing::info!("PostgreSQL 表统计采集到 {} 张表", stats.len());
        Ok(stats)
    }

    async fn query_pg_indexes(
        &self,
        pool: &sqlx::PgPool,
        table_name: &str,
    ) -> Result<Vec<IndexInfo>> {
        let rows: Vec<PgIndexRow> = sqlx::query_as(
            r#"SELECT
                 indexname,
                 indexdef
               FROM pg_indexes
               WHERE tablename = $1"#,
        )
        .bind(table_name)
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let unique = row.indexdef.to_uppercase().contains("UNIQUE");
                // 从 indexdef 中简单提取列名
                let columns = extract_columns_from_indexdef(&row.indexdef);
                IndexInfo {
                    name: row.indexname,
                    columns,
                    unique,
                }
            })
            .collect())
    }
}

// ─── SQL 行映射结构体 ───

#[derive(sqlx::FromRow)]
struct MysqlSlowRow {
    digest_text: String,
    count_star: i64,
    avg_duration_ms: f64,
    sum_rows_examined: i64,
    sum_rows_sent: i64,
}

#[derive(sqlx::FromRow)]
struct MysqlTableRow {
    table_schema: String,
    table_name: String,
    table_rows: Option<i64>,
    data_length: Option<i64>,
    index_length: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct MysqlIndexRow {
    index_name: String,
    column_name: String,
    non_unique: i32,
}

#[derive(sqlx::FromRow)]
struct PgSlowRow {
    query: String,
    calls: i64,
    mean_time_ms: f64,
    rows: i64,
}

#[derive(sqlx::FromRow)]
struct PgTableRow {
    schemaname: String,
    table_name: String,
    row_count: i64,
    total_size: i64,
}

#[derive(sqlx::FromRow)]
struct PgIndexRow {
    indexname: String,
    indexdef: String,
}

/// 从 PostgreSQL indexdef 中提取列名
fn extract_columns_from_indexdef(indexdef: &str) -> Vec<String> {
    // CREATE INDEX idx_name ON table_name USING btree (col1, col2)
    if let Some(start) = indexdef.rfind('(') {
        if let Some(end) = indexdef.rfind(')') {
            let cols = &indexdef[start + 1..end];
            return cols
                .split(',')
                .map(|c| c.trim().to_string())
                .collect();
        }
    }
    vec![]
}
