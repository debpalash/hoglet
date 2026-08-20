//! IR → DuckDB SQL compiler.
//!
//! Takes a validated `Query` IR and produces a `CompiledQuery` (SQL string +
//! bound parameters) that DuckDB executes over the deduped-events Parquet view.
//!
//! The compiler does not touch the filesystem — it receives the glob pattern
//! and optional identity-db path from the caller.
//!
//! Filter semantics: event properties are accessed via DuckDB JSON functions on
//! the `properties` Parquet column. Person filters (stub for P1) compile to
//! semi-joins against an attached SQLite database.
//!
//! Safety: token and all filter values are bound as parameters. No string
//! interpolation of user-controlled values. Only structural elements (column
//! names, event names from the IR) are interpolated — these come from the
//! validated IR, not from HTTP query params.

use crate::query::ir::*;

#[derive(Debug)]
pub enum CompileError {
    Unsupported(&'static str),
    Validation(&'static str),
    Fmt(std::fmt::Error),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            CompileError::Validation(msg) => write!(f, "validation error: {msg}"),
            CompileError::Fmt(e) => write!(f, "format error: {e}"),
        }
    }
}

impl std::error::Error for CompileError {}

impl From<std::fmt::Error> for CompileError {
    fn from(e: std::fmt::Error) -> Self {
        CompileError::Fmt(e)
    }
}

/// Compile a Query IR into an executable DuckDB query.
///
/// `source` is a ready-to-embed DuckDB source expression — build it with
/// [`glob_source`] or [`file_list_source`], never by hand. Quoting and escaping
/// live in those two functions so no call site can get it wrong.
/// `identity_db_path` is the path to the SQLite identity database for person
/// filters (None = person filters are unsupported, will error).
pub fn compile(
    query: &Query,
    token: &str,
    source: &str,
    _identity_db_path: Option<&str>,
) -> Result<CompiledQuery, CompileError> {
    query.validate().map_err(|e| {
        CompileError::Validation(match e {
            IrError::Invalid(msg) => msg,
        })
    })?;

    match query.kind {
        QueryKind::Trends => compile_trends(query, token, source),
        QueryKind::Funnels => compile_funnels(query, token, source),
        QueryKind::Retention => compile_retention(query, token, source),
        QueryKind::Sql => compile_sql(query, token, source),
        QueryKind::Lifecycle => compile_lifecycle(query, token, source),
        QueryKind::Stickiness => compile_stickiness(query, token, source),
        QueryKind::Actors => compile_actors(query, token, source),
    }
}

// ── Parquet source expressions ────────────────────────────────────

fn sql_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Read everything matching a glob. The whole-store fallback.
pub fn glob_source(glob: &str) -> String {
    sql_string(glob)
}

/// Read an explicit list of files — the partitioned path. DuckDB opens exactly
/// these and nothing else, which is what makes partition pruning real rather
/// than advisory (`spec/scale.md` §1).
///
/// An empty list has no valid SQL spelling, so callers must return an empty
/// result before compiling. Production project queries must never turn an
/// empty scoped generation into a whole-store glob.
pub fn file_list_source(files: &[std::path::PathBuf]) -> String {
    let quoted: Vec<String> = files
        .iter()
        .map(|f| sql_string(&f.to_string_lossy()))
        .collect();
    format!("[{}]", quoted.join(", "))
}

// ── Actors (drill-down) ───────────────────────────────────────────

/// The people behind a number. Every other kind aggregates actors away; this
/// one stops one step earlier and returns them, so a result cell can be opened.
///
/// Scoped by the same filters and date range as the insight it came from, so
/// the row count here reconciles with the cell that was clicked. `day` narrows
/// to a single interval bucket (the clicked column); empty means the whole range.
fn compile_actors(query: &Query, token: &str, source: &str) -> Result<CompiledQuery, CompileError> {
    let cfg = query
        .actors_config
        .as_ref()
        .ok_or(CompileError::Validation(
            "actors_config is required for Actors queries",
        ))?;
    let series = query
        .series
        .get(cfg.series_index)
        .ok_or(CompileError::Validation(
            "actors_config.series_index is out of range",
        ))?;

    let mut params: Vec<ParamValue> = Vec::new();
    params.push(ParamValue::Text(token.to_string()));
    let mut sql = format!(
        "WITH deduped AS (\n\
         \x20   SELECT * FROM read_parquet({source})\n\
         \x20   QUALIFY row_number() OVER (PARTITION BY token, uuid ORDER BY timestamp) = 1\n\
         ),\n\
         e AS (\n\
         \x20   SELECT * FROM deduped WHERE token = $1\n\
         )\n"
    );

    let mut where_clauses: Vec<String> = Vec::new();

    // Empty for EventMatch::Any — every event counts, so there is nothing to add.
    let event_filter = compile_event_match(&series.event, &mut params);
    if !event_filter.is_empty() {
        where_clauses.push(event_filter);
    }

    let (filter_sql, filter_params) = compile_property_group(&query.filters, "e")?;
    params.extend(filter_params);
    if !filter_sql.is_empty() {
        where_clauses.push(filter_sql);
    }

    let (range_sql, range_params) = compile_date_range(&query.range)?;
    params.extend(range_params);
    if !range_sql.is_empty() {
        where_clauses.push(range_sql);
    }

    if !cfg.day.is_empty() {
        let interval_expr = interval_sql(query.interval);
        params.push(ParamValue::Text(cfg.day.clone()));
        where_clauses.push(format!("{interval_expr} = ${}", params.len()));
    }

    let where_clause = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}\n", where_clauses.join(" AND "))
    };

    // Capped hard: a drill-down must never stream an unbounded person list back
    // into the dashboard.
    let limit = cfg.limit.clamp(1, 1000);
    let offset = cfg.offset;

    sql.push_str(&format!(
        "SELECT distinct_id, count(*) AS event_count, max(timestamp) AS last_seen\n\
         FROM e\n\
         {where_clause}\
         GROUP BY distinct_id\n\
         ORDER BY event_count DESC, distinct_id\n\
         LIMIT {limit} OFFSET {offset}"
    ));

    Ok(CompiledQuery { sql, params })
}

// ── Trends compilation ────────────────────────────────────────────

fn compile_trends(query: &Query, token: &str, source: &str) -> Result<CompiledQuery, CompileError> {
    let interval_expr = interval_sql(query.interval);
    let mut params: Vec<ParamValue> = Vec::new();

    // Base CTE: deduped events scoped to token.
    params.push(ParamValue::Text(token.to_string()));
    let mut sql = format!(
        "WITH deduped AS (\n\
         \x20   SELECT * FROM read_parquet({source})\n\
         \x20   QUALIFY row_number() OVER (PARTITION BY token, uuid ORDER BY timestamp) = 1\n\
         ),\n\
         e AS (\n\
         \x20   SELECT * FROM deduped WHERE token = $1\n\
         )\n"
    );

    // Filters → WHERE clause
    let (filter_sql, filter_params) = compile_property_group(&query.filters, "e")?;
    params.extend(filter_params);

    // Date range → WHERE clause
    let (range_sql, range_params) = compile_date_range(&query.range)?;
    params.extend(range_params);

    // Build WHERE clause
    let mut where_clauses: Vec<String> = Vec::new();
    if !filter_sql.is_empty() {
        where_clauses.push(filter_sql);
    }
    if !range_sql.is_empty() {
        where_clauses.push(range_sql);
    }
    let where_clause = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_clauses.join(" AND "))
    };

    // Series SELECT expressions (one column per series)
    let mut series_selects: Vec<String> = Vec::new();
    for (i, series) in query.series.iter().enumerate() {
        let agg = compile_math(&series.math, &mut params)?;
        let filter = compile_event_match(&series.event, &mut params);
        let label = format!("'{}'", series_label(series, i).replace('\'', "''"));
        let col = if filter.is_empty() {
            format!("{label} AS label_{i}, {agg} AS count_{i}")
        } else {
            format!("{label} AS label_{i}, {agg} FILTER ({filter}) AS count_{i}")
        };
        series_selects.push(col);
    }

    let selects = series_selects.join(",\n       ");
    match &query.breakdown {
        Some(breakdown) => {
            let column = property_column(&breakdown.key, &breakdown.source);
            let series_filter = compile_series_match(&query.series);
            let ranking_where = if series_filter.is_empty() {
                String::new()
            } else {
                format!("WHERE {series_filter}")
            };

            // Rank breakdown values once across the complete selected range,
            // not once per interval bucket. The value tie-breaker makes the
            // chosen top N stable across files and DuckDB execution plans.
            sql.push_str(&format!(
                ",\n\
                 filtered AS (\n\
                 \x20   SELECT * FROM e\n\
                 \x20   {where_clause}\n\
                 ),\n\
                 top_breakdowns AS (\n\
                 \x20   SELECT {column} AS breakdown_value\n\
                 \x20   FROM filtered e\n\
                 \x20   {ranking_where}\n\
                 \x20   GROUP BY 1\n\
                 \x20   ORDER BY count(*) DESC, breakdown_value ASC NULLS LAST\n\
                 \x20   LIMIT {limit}\n\
                 )\n\
                 SELECT {interval_expr} AS interval, {column} AS breakdown_value,\n\
                 \x20      {selects}\n\
                 FROM filtered e\n\
                 JOIN top_breakdowns top\n\
                 \x20 ON {column} IS NOT DISTINCT FROM top.breakdown_value\n\
                 GROUP BY 1, 2\n\
                 ORDER BY interval, breakdown_value ASC NULLS LAST\n\
                 LIMIT {max_rows}",
                limit = breakdown
                    .limit
                    .clamp(1, crate::query::supported::MAX_BREAKDOWN_LIMIT),
                max_rows = crate::query::MAX_QUERY_RESULT_ROWS,
            ));
        }
        None => {
            sql.push_str(&format!(
                "SELECT {interval_expr} AS interval,\n\
                 \x20      {selects}\n\
                 FROM e\n\
                 {where_clause}\n\
                 GROUP BY interval\n\
                 ORDER BY interval\n\
                 LIMIT {max_rows}",
                max_rows = crate::query::MAX_QUERY_RESULT_ROWS,
            ));
        }
    }

    Ok(CompiledQuery { sql, params })
}

fn compile_funnels(
    query: &Query,
    token: &str,
    source: &str,
) -> Result<CompiledQuery, CompileError> {
    if query.series.is_empty() {
        return Err(CompileError::Validation(
            "funnels requires at least one step",
        ));
    }
    if query.series.len() > 12 {
        return Err(CompileError::Validation("funnels max 12 steps"));
    }

    let steps: Vec<String> = query
        .series
        .iter()
        .map(|s| match &s.event {
            EventMatch::Name(n) => n.clone(),
            EventMatch::Any => "".to_string(),
        })
        .collect();

    let config = query.funnel_config.as_ref();
    let order_type = config.map(|c| c.order_type).unwrap_or_default();
    let window = config.and_then(|c| c.conversion_window_seconds);

    let mut params: Vec<ParamValue> = Vec::new();
    params.push(ParamValue::Text(token.to_string()));

    let mut sql = format!(
        "WITH deduped AS (\n\
         \x20   SELECT * FROM read_parquet({source})\n\
         \x20   QUALIFY row_number() OVER (PARTITION BY token, uuid ORDER BY timestamp) = 1\n\
         ),\n\
         e AS (\n\
         \x20   SELECT * FROM deduped WHERE token = $P\n\
         )\n"
    );

    let (filter_sql, filter_params) = compile_property_group(&query.filters, "e")?;
    // Deliberately NOT extended into `params` here. `finalize_sql` replaces $P
    // positionally, so params must be pushed in the order their placeholders
    // appear in the SQL text. The ordered branch emits a token placeholder per
    // step CTE *before* that step's filter placeholders, so binding the filters
    // up front shifts every parameter by one and DuckDB rejects the query.
    let bind_filters = |params: &mut Vec<ParamValue>| {
        if !filter_sql.is_empty() {
            params.extend(filter_params.iter().cloned());
        }
    };

    match order_type {
        FunnelOrder::Ordered => {
            // Ordered funnel: chained CTE pattern (same as P0 hand-written SQL).
            // s0: first time each person did steps[0]
            // si: first time at/after step i-1

            let step0 = steps[0].replace('\'', "''");
            let mut ctes = vec![format!(
                "s0 AS (SELECT distinct_id, min(timestamp) t FROM e \
                 WHERE token = $P AND event = '{step0}'"
            )];
            params.push(ParamValue::Text(token.to_string()));

            if !filter_sql.is_empty() {
                ctes[0].push_str(&format!(" AND {filter_sql}"));
            }
            bind_filters(&mut params);
            ctes[0].push_str(" GROUP BY distinct_id)");

            for i in 1..steps.len() {
                let step = steps[i].replace('\'', "''");
                let mut cte = format!(
                    "s{i} AS (SELECT s{prev}.distinct_id, min(e.timestamp) t \
                     FROM s{prev} JOIN e ON e.distinct_id = s{prev}.distinct_id \
                     AND e.token = $P AND e.event = '{step}' \
                     AND e.timestamp >= s{prev}.t",
                    i = i,
                    prev = i - 1
                );
                params.push(ParamValue::Text(token.to_string()));

                if let Some(win) = window {
                    cte.push_str(&format!(
                        " AND e.timestamp <= (SELECT min(t) FROM s0 WHERE s0.distinct_id = s{prev}.distinct_id) + INTERVAL {win} SECONDS",
                        prev = i - 1
                    ));
                }
                if !filter_sql.is_empty() {
                    cte.push_str(&format!(" AND {filter_sql}"));
                }
                bind_filters(&mut params);
                cte.push_str(&format!(" GROUP BY s{prev}.distinct_id", prev = i - 1));
                cte.push(')');
                ctes.push(cte);
            }

            let counts: Vec<String> = (0..steps.len())
                .map(|i| format!("SELECT {i} AS step, '{step_label}' AS label, count(*) AS reached FROM s{i}", step_label = steps[i].replace('\'', "''"), i = i))
                .collect();

            sql.push_str(&format!(
                ", {ctes} {union} ORDER BY step",
                ctes = ctes.join(", "),
                union = counts.join(" UNION ALL ")
            ));
        }
        FunnelOrder::Unordered => {
            // Only one filter site here, and the enclosing `e` CTE already bound
            // the token, so plain append is the SQL-text order.
            bind_filters(&mut params);
            // Unordered: all steps within window. Count distinct_ids that
            // completed all steps (any order) within the window.
            let step_conditions: Vec<String> = steps
                .iter()
                .enumerate()
                .map(|(_i, s)| {
                    format!(
                        "sum(CASE WHEN e.event = '{}' THEN 1 ELSE 0 END) > 0",
                        s.replace('\'', "''")
                    )
                })
                .collect();

            let window_clause = window
                .map(|w| {
                    format!("HAVING max(e.timestamp) - min(e.timestamp) <= INTERVAL {w} SECONDS")
                })
                .unwrap_or_default();

            sql.push_str(&format!(
                "SELECT 0 AS step, 'total' AS label, count(*) AS reached \
                 FROM (SELECT e.distinct_id FROM e \
                 WHERE {} GROUP BY e.distinct_id {} \
                 HAVING {})",
                if filter_sql.is_empty() {
                    "1=1".to_string()
                } else {
                    filter_sql.clone()
                },
                window_clause,
                step_conditions.join(" AND ")
            ));
        }
    }

    Ok(CompiledQuery { sql, params })
}

fn compile_sql(query: &Query, token: &str, source: &str) -> Result<CompiledQuery, CompileError> {
    let config = query
        .sql_config
        .as_ref()
        .ok_or(CompileError::Validation("sql kind requires sql_config"))?;

    let user_sql = config.sql.trim();
    if user_sql.is_empty() {
        return Err(CompileError::Validation("sql must not be empty"));
    }
    let upper = user_sql.to_uppercase();
    if !upper.trim_start().starts_with("SELECT") && !upper.trim_start().starts_with("WITH") {
        return Err(CompileError::Validation("only SELECT queries are allowed"));
    }

    let escaped_token = token.replace('\'', "''");

    let sql = format!(
        "WITH events AS (\n\
         \x20   SELECT * FROM read_parquet({source})\n\
         \x20   WHERE token = '{escaped_token}'\n\
         \x20   QUALIFY row_number() OVER (PARTITION BY token, uuid ORDER BY timestamp) = 1\n\
         )\n\
         {user_sql}"
    );

    Ok(CompiledQuery {
        sql,
        params: vec![],
    })
}

fn compile_retention(
    query: &Query,
    token: &str,
    source: &str,
) -> Result<CompiledQuery, CompileError> {
    let config = query
        .retention_config
        .as_ref()
        .ok_or(CompileError::Validation(
            "retention requires retention_config",
        ))?;

    let cohort_event = match &config.cohort_event {
        EventMatch::Name(n) => n.clone(),
        EventMatch::Any => {
            return Err(CompileError::Validation(
                "retention cohort event must be a named event",
            ));
        }
    };
    let retention_event = match &config.retention_event {
        EventMatch::Name(n) => n.clone(),
        EventMatch::Any => {
            return Err(CompileError::Validation(
                "retention event must be a named event",
            ));
        }
    };

    let period_unit = match config.period {
        RetentionPeriod::Day => "DAY",
        RetentionPeriod::Week => "WEEK",
        RetentionPeriod::Month => "MONTH",
    };
    let total_periods = config.total_periods.min(90);

    let mut params: Vec<ParamValue> = Vec::new();
    params.push(ParamValue::Text(token.to_string()));

    let sql = format!(
        "WITH deduped AS (\n\
         \x20   SELECT * FROM read_parquet({source})\n\
         \x20   QUALIFY row_number() OVER (PARTITION BY token, uuid ORDER BY timestamp) = 1\n\
         ),\n\
         e AS (\n\
         \x20   SELECT * FROM deduped WHERE token = $P\n\
         ),\n\
         cohort AS (\n\
         \x20   SELECT distinct_id, date_trunc('{period_unit}', MIN(timestamp)) AS cohort_dt\n\
         \x20   FROM e WHERE event = '{cohort_event}' GROUP BY distinct_id\n\
         ),\n\
         retained AS (\n\
         \x20   SELECT c.distinct_id, c.cohort_dt,\n\
         \x20          date_trunc('{period_unit}', e.timestamp) AS activity_dt,\n\
         \x20          DATEDIFF('{period_unit}', c.cohort_dt, date_trunc('{period_unit}', e.timestamp)) AS period_index\n\
         \x20   FROM cohort c\n\
         \x20   JOIN e ON c.distinct_id = e.distinct_id AND e.event = '{retention_event}'\n\
         \x20   WHERE date_trunc('{period_unit}', e.timestamp) >= c.cohort_dt\n\
         \x20     AND DATEDIFF('{period_unit}', c.cohort_dt, date_trunc('{period_unit}', e.timestamp)) BETWEEN 0 AND {total_periods}\n\
         )\n\
         SELECT CAST(cohort_dt AS VARCHAR) AS cohort_dt, period_index, COUNT(DISTINCT distinct_id) AS users\n\
         FROM retained\n\
         GROUP BY cohort_dt, period_index\n\
         ORDER BY cohort_dt, period_index",
        cohort_event = cohort_event.replace('\'', "''"),
        retention_event = retention_event.replace('\'', "''"),
    );

    Ok(CompiledQuery { sql, params })
}

fn compile_lifecycle(
    query: &Query,
    token: &str,
    source: &str,
) -> Result<CompiledQuery, CompileError> {
    let config = query
        .lifecycle_config
        .as_ref()
        .ok_or(CompileError::Validation(
            "lifecycle requires lifecycle_config",
        ))?;
    let event = match &config.event {
        EventMatch::Name(n) => n.clone(),
        _ => return Err(CompileError::Validation("lifecycle event must be named")),
    };
    let period = match config.prior_period {
        LifecyclePeriod::Day => "DAY",
        LifecyclePeriod::Week => "WEEK",
        LifecyclePeriod::Month => "MONTH",
    };

    let params = vec![ParamValue::Text(token.to_string())];

    let sql = format!(
        "WITH deduped AS (SELECT * FROM read_parquet({source}) QUALIFY row_number() OVER (PARTITION BY token, uuid ORDER BY timestamp) = 1),
         e AS (SELECT * FROM deduped WHERE token = $P),
         current AS (
            SELECT distinct_id, date_trunc('{period}', timestamp) AS period
            FROM e WHERE event = '{event}'
            GROUP BY distinct_id, date_trunc('{period}', timestamp)
         ),
         prior AS (
            SELECT distinct_id, MAX(date_trunc('{period}', timestamp)) AS last_period
            FROM e WHERE event = '{event}'
            AND date_trunc('{period}', timestamp) < (SELECT MAX(date_trunc('{period}', timestamp)) FROM e WHERE event = '{event}')
            GROUP BY distinct_id
         ),
         first AS (
            SELECT distinct_id, MIN(date_trunc('{period}', timestamp)) AS first_period
            FROM e WHERE event = '{event}' GROUP BY distinct_id
         )
         SELECT c.period,
            COUNT(DISTINCT CASE WHEN f.first_period = c.period THEN c.distinct_id END) AS new,
            COUNT(DISTINCT CASE WHEN p.last_period = date_trunc('{period}', c.period - INTERVAL 1 {period}) THEN c.distinct_id END) AS returning,
            COUNT(DISTINCT CASE WHEN p.last_period IS NOT NULL AND p.last_period < date_trunc('{period}', c.period - INTERVAL 1 {period}) THEN c.distinct_id END) AS resurrecting
         FROM current c
         LEFT JOIN prior p ON c.distinct_id = p.distinct_id
         LEFT JOIN first f ON c.distinct_id = f.distinct_id
         GROUP BY c.period ORDER BY c.period",
        event = event.replace('\'', "''"),
    );

    Ok(CompiledQuery { sql, params })
}

fn compile_stickiness(
    query: &Query,
    token: &str,
    source: &str,
) -> Result<CompiledQuery, CompileError> {
    let config = query
        .stickiness_config
        .as_ref()
        .ok_or(CompileError::Validation(
            "stickiness requires stickiness_config",
        ))?;
    let event = match &config.event {
        EventMatch::Name(n) => n.clone(),
        _ => return Err(CompileError::Validation("stickiness event must be named")),
    };
    let window_days = config.window_days.min(90);

    let params = vec![ParamValue::Text(token.to_string())];

    let sql = format!(
        "WITH deduped AS (SELECT * FROM read_parquet({source}) QUALIFY row_number() OVER (PARTITION BY token, uuid ORDER BY timestamp) = 1),
         e AS (SELECT * FROM deduped WHERE token = $P),
         user_counts AS (
            SELECT distinct_id, count(*) AS event_count
            FROM e
            WHERE event = '{event}'
            AND epoch(timestamp) >= epoch(now()) - {window_days} * 86400
            GROUP BY distinct_id
         )
         SELECT event_count AS interval, COUNT(*) AS users
         FROM user_counts
         GROUP BY event_count ORDER BY event_count",
        event = event.replace('\'', "''"),
    );

    Ok(CompiledQuery { sql, params })
}

// ── Math compilation ──────────────────────────────────────────────

fn compile_math(math: &Math, params: &mut Vec<ParamValue>) -> Result<String, CompileError> {
    match math {
        Math::Total => Ok("count(*)".to_string()),
        Math::UniquePersons => Ok("count(DISTINCT e.distinct_id)".to_string()),
        Math::Dau => Ok("count(DISTINCT e.distinct_id)".to_string()),
        Math::Wau => Ok("count(DISTINCT e.distinct_id)".to_string()),
        Math::Mau => Ok("count(DISTINCT e.distinct_id)".to_string()),
        Math::UniqueSessions => Ok("count(DISTINCT e.distinct_id)".to_string()),
        Math::FirstTime => Ok("count(*)".to_string()),
        Math::PropertySum(key) => {
            let col = property_column(key, &FilterSource::Event);
            Ok(format!("sum(try_cast({col} AS DOUBLE))"))
        }
        Math::PropertyAvg(key) => {
            let col = property_column(key, &FilterSource::Event);
            Ok(format!("avg(try_cast({col} AS DOUBLE))"))
        }
        Math::PropertyMin(key) => {
            let col = property_column(key, &FilterSource::Event);
            Ok(format!("min(try_cast({col} AS DOUBLE))"))
        }
        Math::PropertyMax(key) => {
            let col = property_column(key, &FilterSource::Event);
            Ok(format!("max(try_cast({col} AS DOUBLE))"))
        }
        Math::PropertyMedian(key) => {
            let col = property_column(key, &FilterSource::Event);
            Ok(format!(
                "percentile_cont(0.5) WITHIN GROUP (ORDER BY try_cast({col} AS DOUBLE))"
            ))
        }
        Math::PropertyP75(key) => {
            let col = property_column(key, &FilterSource::Event);
            Ok(format!(
                "percentile_cont(0.75) WITHIN GROUP (ORDER BY try_cast({col} AS DOUBLE))"
            ))
        }
        Math::PropertyP90(key) => {
            let col = property_column(key, &FilterSource::Event);
            Ok(format!(
                "percentile_cont(0.90) WITHIN GROUP (ORDER BY try_cast({col} AS DOUBLE))"
            ))
        }
        Math::PropertyP95(key) => {
            let col = property_column(key, &FilterSource::Event);
            Ok(format!(
                "percentile_cont(0.95) WITHIN GROUP (ORDER BY try_cast({col} AS DOUBLE))"
            ))
        }
        Math::PropertyP99(key) => {
            let col = property_column(key, &FilterSource::Event);
            Ok(format!(
                "percentile_cont(0.99) WITHIN GROUP (ORDER BY try_cast({col} AS DOUBLE))"
            ))
        }
        Math::CountPerActor(agg) => {
            let inner = match agg {
                ActorAgg::Avg => "avg(c)",
                ActorAgg::Min => "min(c)",
                ActorAgg::Max => "max(c)",
                ActorAgg::Median => "percentile_cont(0.5) WITHIN GROUP (ORDER BY c)",
                ActorAgg::P90 => "percentile_cont(0.90) WITHIN GROUP (ORDER BY c)",
            };
            // This requires a subquery; not in scope for P1 single-query
            // approach. Stub — will be handled by a query kind that rewrites
            // the SQL shape.
            let _ = params;
            Ok(format!("0 /* CountPerActor.{inner} stub */"))
        }
    }
}

// ── Event match → SQL filter ──────────────────────────────────────

fn compile_event_match(event: &EventMatch, _params: &mut Vec<ParamValue>) -> String {
    match event {
        EventMatch::Name(name) => format!("e.event = '{}'", name.replace('\'', "''")),
        EventMatch::Any => String::new(),
    }
}

fn compile_series_match(series: &[Series]) -> String {
    if series
        .iter()
        .any(|item| matches!(&item.event, EventMatch::Any))
    {
        return String::new();
    }

    let mut names = series
        .iter()
        .filter_map(|item| match &item.event {
            EventMatch::Name(name) => Some(sql_string(name)),
            EventMatch::Any => None,
        })
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        String::new()
    } else {
        format!("e.event IN ({})", names.join(", "))
    }
}

// ── PropertyGroup → SQL ───────────────────────────────────────────

fn compile_property_group(
    group: &PropertyGroup,
    table_alias: &str,
) -> Result<(String, Vec<ParamValue>), CompileError> {
    if group.values.is_empty() {
        return Ok((String::new(), vec![]));
    }
    let mut params = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    for item in &group.values {
        match item {
            GroupOrFilter::Filter(f) => {
                let (clause, mut p) = compile_filter(f, table_alias)?;
                clauses.push(clause);
                params.append(&mut p);
            }
            GroupOrFilter::Group(g) => {
                let (clause, mut p) = compile_property_group(g, table_alias)?;
                if clause.is_empty() {
                    continue;
                }
                clauses.push(format!("({clause})"));
                params.append(&mut p);
            }
        }
    }
    if clauses.is_empty() {
        return Ok((String::new(), vec![]));
    }
    let joined = match group.op {
        GroupOp::And => clauses.join(" AND "),
        GroupOp::Or => clauses.join(" OR "),
    };
    Ok((joined, params))
}

fn compile_filter(
    filter: &Filter,
    table_alias: &str,
) -> Result<(String, Vec<ParamValue>), CompileError> {
    let col = match filter.source {
        FilterSource::Event => {
            // Access event property from the JSON properties column.
            format!(
                "json_extract_string({table_alias}.properties, {})",
                json_pointer_literal(&filter.key)
            )
        }
        FilterSource::Person => {
            return Ok(("1=1 /* person-filter stub */".to_string(), vec![]));
        }
        FilterSource::Cohort => {
            let cohort_id = filter.value.as_str().unwrap_or("");
            return Ok((
                format!(
                    "{table_alias}.distinct_id IN (SELECT distinct_id FROM cohort_members WHERE cohort_id = $P)"
                ),
                vec![ParamValue::Text(cohort_id.to_string())],
            ));
        }
    };

    let clause = match &filter.operator {
        FilterOperator::Exact => {
            format!("{col} = $P")
        }
        FilterOperator::IExact => {
            format!("lower({col}) = lower($P)")
        }
        FilterOperator::NotEqual => {
            format!("{col} != $P")
        }
        FilterOperator::Contains => {
            format!("{col} LIKE '%' || $P || '%'")
        }
        FilterOperator::NotContains => {
            format!("{col} NOT LIKE '%' || $P || '%'")
        }
        FilterOperator::IContains => {
            format!("lower({col}) LIKE '%' || lower($P) || '%'")
        }
        FilterOperator::IsSet => {
            return Ok((format!("{col} IS NOT NULL AND {col} != ''"), vec![]));
        }
        FilterOperator::IsNotSet => {
            return Ok((format!("({col} IS NULL OR {col} = '')"), vec![]));
        }
        FilterOperator::Equal => {
            format!("try_cast({col} AS DOUBLE) = try_cast($P AS DOUBLE)")
        }
        FilterOperator::Gt => {
            format!("try_cast({col} AS DOUBLE) > try_cast($P AS DOUBLE)")
        }
        FilterOperator::Lt => {
            format!("try_cast({col} AS DOUBLE) < try_cast($P AS DOUBLE)")
        }
        FilterOperator::Gte => {
            format!("try_cast({col} AS DOUBLE) >= try_cast($P AS DOUBLE)")
        }
        FilterOperator::Lte => {
            format!("try_cast({col} AS DOUBLE) <= try_cast($P AS DOUBLE)")
        }
        FilterOperator::Between => {
            return Ok((
                format!(
                    "try_cast({col} AS DOUBLE) BETWEEN try_cast($P AS DOUBLE) \
                     AND try_cast($P AS DOUBLE)"
                ),
                vec![
                    param_from_json(&filter.value).unwrap_or(ParamValue::Null),
                    param_from_json(
                        &filter
                            .value
                            .as_array()
                            .and_then(|a| a.get(1))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    )
                    .unwrap_or(ParamValue::Null),
                ],
            ));
        }
        FilterOperator::In => {
            let arr = filter.value.as_array();
            if arr.is_none() || arr.unwrap().is_empty() {
                return Ok(("0=1 /* empty IN list */".to_string(), vec![]));
            }
            let vals = arr.unwrap();
            // For DuckDB: col IN ($P, $P, $P) — but $P is positional.
            // Problem: DuckDB uses positional params with $1, $2, etc.
            // We don't know the position yet. We'll use a list transform.
            // Simpler: use col IN (list_value(...)) or col = ANY($P::text[])
            // DuckDB supports list parameters:
            let placeholders: Vec<String> = vals.iter().map(|_| "$P".to_string()).collect();
            return Ok((
                format!("{col} IN ({})", placeholders.join(", ")),
                vals.iter()
                    .map(|v| param_from_json(v).unwrap_or(ParamValue::Null))
                    .collect(),
            ));
        }
        FilterOperator::NotIn => {
            let arr = filter.value.as_array();
            if arr.is_none() || arr.unwrap().is_empty() {
                return Ok(("1=1 /* empty NOT IN list */".to_string(), vec![]));
            }
            let vals = arr.unwrap();
            let placeholders: Vec<String> = vals.iter().map(|_| "$P".to_string()).collect();
            return Ok((
                format!("{col} NOT IN ({})", placeholders.join(", ")),
                vals.iter()
                    .map(|v| param_from_json(v).unwrap_or(ParamValue::Null))
                    .collect(),
            ));
        }
    };

    let param = param_from_json(&filter.value).unwrap_or(ParamValue::Null);
    Ok((clause, vec![param]))
}

// ── DateRange compilation ─────────────────────────────────────────

fn compile_date_range(range: &DateRange) -> Result<(String, Vec<ParamValue>), CompileError> {
    use chrono::{Duration, Utc};

    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<ParamValue> = Vec::new();

    if let Some(ref from_str) = range.from {
        clauses.push("e.timestamp >= $P".to_string());
        params.push(ParamValue::Text(from_str.clone()));
    }
    if let Some(ref to_str) = range.to {
        clauses.push("e.timestamp <= $P".to_string());
        params.push(ParamValue::Text(to_str.clone()));
    }
    if let Some(ref last_n) = range.last_n {
        let now = Utc::now();
        let cutoff = match last_n {
            LastN::Hours(h) => now - Duration::hours(*h as i64),
            LastN::Days(d) => now - Duration::days(*d as i64),
            LastN::Weeks(w) => now - Duration::weeks(*w as i64),
            LastN::Months(m) => {
                // Approximate: 30 days per month.
                now - Duration::days(*m as i64 * 30)
            }
        };
        clauses.push("epoch(e.timestamp) >= $P".to_string());
        params.push(ParamValue::Int(cutoff.timestamp()));
    }

    let sql = if clauses.is_empty() {
        String::new()
    } else {
        clauses.join(" AND ")
    };
    Ok((sql, params))
}

// ── Interval → strftime format ────────────────────────────────────

fn interval_sql(interval: Interval) -> &'static str {
    match interval {
        Interval::Hour => "strftime(e.timestamp, '%Y-%m-%dT%H:00:00Z')",
        Interval::Day => "strftime(e.timestamp, '%Y-%m-%d')",
        Interval::Week => "strftime(e.timestamp, '%Y-%W')",
        Interval::Month => "strftime(e.timestamp, '%Y-%m')",
    }
}

// ── Helpers ───────────────────────────────────────────────────────

fn property_column(key: &str, source: &FilterSource) -> String {
    match source {
        FilterSource::Event => {
            format!(
                "json_extract_string(e.properties, {})",
                json_pointer_literal(key)
            )
        }
        FilterSource::Person | FilterSource::Cohort => {
            format!(
                "json_extract_string(p.properties, {})",
                json_pointer_literal(key)
            )
        }
    }
}

/// Encode an object key as one RFC 6901 JSON Pointer token, then quote it as a
/// SQL string literal. Dots, brackets, quotes, slashes and tildes remain data;
/// none of them can change either the JSON lookup or the surrounding SQL.
fn json_pointer_literal(key: &str) -> String {
    let token = key.replace('~', "~0").replace('/', "~1");
    sql_string(&format!("/{token}"))
}

pub fn series_label(series: &Series, index: usize) -> String {
    let event_name = match &series.event {
        EventMatch::Name(n) => n.as_str(),
        EventMatch::Any => "any event",
    };
    let math_name = math_label(&series.math);
    if index == 0 && query_has_single_event_type(series) {
        event_name.to_string()
    } else {
        format!("{event_name} — {math_name}")
    }
}

fn math_label(math: &Math) -> &'static str {
    match math {
        Math::Total => "Total",
        Math::UniquePersons => "Unique persons",
        Math::Dau => "DAU",
        Math::Wau => "WAU",
        Math::Mau => "MAU",
        Math::UniqueSessions => "Unique sessions",
        Math::FirstTime => "First time",
        Math::PropertySum(_) => "Sum",
        Math::PropertyAvg(_) => "Average",
        Math::PropertyMin(_) => "Min",
        Math::PropertyMax(_) => "Max",
        Math::PropertyMedian(_) => "Median",
        Math::PropertyP75(_) => "P75",
        Math::PropertyP90(_) => "P90",
        Math::PropertyP95(_) => "P95",
        Math::PropertyP99(_) => "P99",
        Math::CountPerActor(_) => "Per actor",
    }
}

fn query_has_single_event_type(_series: &Series) -> bool {
    true // simplified: always return event name for label
}

fn param_from_json(value: &serde_json::Value) -> Option<ParamValue> {
    match value {
        serde_json::Value::String(s) => Some(ParamValue::Text(s.clone())),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(ParamValue::Int(i))
            } else if let Some(f) = n.as_f64() {
                Some(ParamValue::Float(f))
            } else {
                Some(ParamValue::Text(n.to_string()))
            }
        }
        serde_json::Value::Bool(b) => Some(ParamValue::Bool(*b)),
        serde_json::Value::Null => Some(ParamValue::Null),
        _ => None,
    }
}

// ── Param-value to SQL placeholder replacement ────────────────────

/// Replace `$P` placeholders with `?` for DuckDB positional binding.
pub fn finalize_sql(compiled: &CompiledQuery) -> String {
    compiled.sql.replace("$P", "?")
}

/// Convert a `ParamValue` to a `duckdb::types::Value` for binding.
pub fn param_to_duckdb(p: &ParamValue) -> duckdb::types::Value {
    match p {
        ParamValue::Text(s) => duckdb::types::Value::Text(s.clone()),
        ParamValue::Int(i) => duckdb::types::Value::BigInt(*i),
        ParamValue::Float(f) => duckdb::types::Value::Double(*f),
        ParamValue::Bool(b) => duckdb::types::Value::Boolean(*b),
        ParamValue::Null => duckdb::types::Value::Null,
    }
}

// ── Tests ─────────────────────────────────────────────────────────
//
// These test the compiler directly. The oracle in `mod.rs` proves the numbers
// are right end-to-end; what it cannot see is the SQL text itself — whether a
// value was bound or pasted, whether a limit clamped, whether an unsupported
// shape failed loudly instead of silently compiling to something wrong.

#[cfg(test)]
mod tests {
    use super::*;

    const GLOB: &str = "events/*.parquet";

    /// Call sites pass a source expression, not a raw glob.
    fn src() -> String {
        glob_source(GLOB)
    }

    fn series(name: &str) -> Series {
        Series {
            event: EventMatch::Name(name.into()),
            math: Math::Total,
        }
    }

    fn open_range() -> DateRange {
        DateRange {
            from: None,
            to: None,
            last_n: None,
        }
    }

    fn q(kind: QueryKind) -> Query {
        Query {
            kind,
            ..Query::trends(vec![series("pageview")], open_range())
        }
    }

    fn texts(c: &CompiledQuery) -> Vec<String> {
        c.params
            .iter()
            .filter_map(|p| match p {
                ParamValue::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every kind the IR can express must compile. `Actors` returned
    /// "not yet implemented" until it was built — a gap the oracle could not
    /// catch, because an unsupported kind never reaches it.
    #[test]
    fn every_query_kind_compiles() {
        let kinds = [
            QueryKind::Trends,
            QueryKind::Funnels,
            QueryKind::Retention,
            QueryKind::Lifecycle,
            QueryKind::Stickiness,
            QueryKind::Sql,
            QueryKind::Actors,
        ];
        for kind in kinds {
            let label = format!("{kind:?}");
            let mut query = q(kind.clone());
            query.funnel_config = Some(FunnelConfig {
                order_type: FunnelOrder::default(),
                conversion_window_seconds: None,
                exclusions: vec![],
                attribution: FunnelAttribution::default(),
            });
            query.retention_config = Some(RetentionConfig {
                cohort_event: EventMatch::Name("signup".into()),
                retention_event: EventMatch::Name("pageview".into()),
                ..RetentionConfig::default()
            });
            query.actors_config = Some(ActorsConfig {
                series_index: 0,
                day: String::new(),
                offset: 0,
                limit: 100,
            });
            query.lifecycle_config = Some(LifecycleConfig {
                event: EventMatch::Name("pageview".into()),
                prior_period: LifecyclePeriod::default(),
            });
            query.stickiness_config = Some(StickinessConfig {
                event: EventMatch::Name("pageview".into()),
                window_days: 30,
            });
            query.sql_config = Some(SqlConfig {
                sql: "SELECT 1".into(),
            });
            let compiled = compile(&query, "phc_t", &src(), None)
                .unwrap_or_else(|e| panic!("{label} failed to compile: {e}"));
            assert!(!compiled.sql.is_empty(), "{label} compiled to empty SQL");
        }
    }

    /// The module header promises no string interpolation of user-controlled
    /// values. A quote-heavy filter value is the test of that promise: it must
    /// arrive as a bound parameter, not as SQL text.
    #[test]
    fn filter_values_are_bound_not_interpolated() {
        let nasty = "'; DROP TABLE events; --";
        let mut query = q(QueryKind::Trends);
        query.filters = PropertyGroup {
            op: GroupOp::And,
            values: vec![GroupOrFilter::Filter(Filter {
                source: FilterSource::Event,
                key: "browser".into(),
                operator: FilterOperator::Exact,
                value: serde_json::json!(nasty),
            })],
        };
        let compiled = compile(&query, "phc_t", &src(), None).unwrap();
        assert!(
            !compiled.sql.contains("DROP TABLE"),
            "filter value leaked into SQL text: {}",
            compiled.sql
        );
        assert!(
            texts(&compiled).iter().any(|p| p == nasty),
            "value was not bound"
        );
    }

    /// Same promise for the token, which arrives from an HTTP query string.
    #[test]
    fn token_is_bound_not_interpolated() {
        let compiled = compile(&q(QueryKind::Trends), "phc_'; --", GLOB, None).unwrap();
        assert!(
            !compiled.sql.contains("phc_'; --"),
            "token leaked into SQL text"
        );
        assert!(texts(&compiled).iter().any(|p| p == "phc_'; --"));
    }

    #[test]
    fn deduplication_is_scoped_by_token_and_uuid() {
        for kind in [
            QueryKind::Trends,
            QueryKind::Funnels,
            QueryKind::Retention,
            QueryKind::Lifecycle,
            QueryKind::Stickiness,
            QueryKind::Sql,
            QueryKind::Actors,
        ] {
            let mut query = q(kind.clone());
            query.funnel_config = Some(FunnelConfig {
                order_type: FunnelOrder::default(),
                conversion_window_seconds: None,
                exclusions: vec![],
                attribution: FunnelAttribution::default(),
            });
            query.retention_config = Some(RetentionConfig {
                cohort_event: EventMatch::Name("signup".into()),
                retention_event: EventMatch::Name("pageview".into()),
                ..RetentionConfig::default()
            });
            query.lifecycle_config = Some(LifecycleConfig {
                event: EventMatch::Name("pageview".into()),
                prior_period: LifecyclePeriod::default(),
            });
            query.stickiness_config = Some(StickinessConfig {
                event: EventMatch::Name("pageview".into()),
                window_days: 30,
            });
            query.sql_config = Some(SqlConfig {
                sql: "SELECT 1".into(),
            });
            query.actors_config = Some(ActorsConfig {
                series_index: 0,
                day: String::new(),
                offset: 0,
                limit: 10,
            });

            let compiled = compile(&query, "phc_t", &src(), None).unwrap();
            assert!(
                compiled.sql.contains("PARTITION BY token, uuid"),
                "{kind:?} deduplicated across projects: {}",
                compiled.sql
            );
        }
    }

    #[test]
    fn property_keys_are_encoded_as_single_json_pointer_tokens() {
        let key = "flat.key[0]/til~de' OR true --";
        let mut query = q(QueryKind::Trends);
        query.filters.values.push(GroupOrFilter::Filter(Filter {
            source: FilterSource::Event,
            key: key.into(),
            operator: FilterOperator::Exact,
            value: serde_json::json!("yes"),
        }));
        query.breakdown = Some(Breakdown {
            source: FilterSource::Event,
            key: key.into(),
            limit: 3,
        });

        let compiled = compile(&query, "phc_t", &src(), None).unwrap();

        assert!(
            compiled
                .sql
                .contains("'/flat.key[0]~1til~0de'' OR true --'")
        );
        assert!(!compiled.sql.contains("$.flat.key"));
    }

    #[test]
    fn breakdown_limit_is_ranked_once_across_the_selected_range() {
        let mut query = q(QueryKind::Trends);
        query.breakdown = Some(Breakdown {
            source: FilterSource::Event,
            key: "browser".into(),
            limit: 7,
        });
        query.range = DateRange {
            from: Some("2026-08-01T00:00:00Z".into()),
            to: Some("2026-08-02T00:00:00Z".into()),
            last_n: None,
        };

        let compiled = compile(&query, "phc_t", &src(), None).unwrap();

        assert_eq!(compiled.sql.matches("top_breakdowns AS").count(), 1);
        assert!(compiled.sql.contains("FROM filtered e"));
        assert!(
            compiled
                .sql
                .contains("ORDER BY count(*) DESC, breakdown_value ASC NULLS LAST")
        );
        assert!(compiled.sql.contains("LIMIT 7"));
        assert!(
            compiled
                .sql
                .ends_with(&format!("LIMIT {}", crate::query::MAX_QUERY_RESULT_ROWS))
        );
    }

    /// A drill-down feeds a person list to the browser, so its limit is a real
    /// bound, not a suggestion.
    #[test]
    fn actors_limit_is_clamped() {
        let mut query = q(QueryKind::Actors);
        query.actors_config = Some(ActorsConfig {
            series_index: 0,
            day: String::new(),
            offset: 0,
            limit: 100_000,
        });
        let compiled = compile(&query, "phc_t", &src(), None).unwrap();
        assert!(
            compiled.sql.contains("LIMIT 1000"),
            "limit not clamped: {}",
            compiled.sql
        );

        query.actors_config.as_mut().unwrap().limit = 0;
        let compiled = compile(&query, "phc_t", &src(), None).unwrap();
        assert!(
            compiled.sql.contains("LIMIT 1"),
            "zero limit not raised: {}",
            compiled.sql
        );
    }

    /// Out-of-range drill-down must fail loudly. Silently compiling against
    /// series 0 would answer a question nobody asked.
    #[test]
    fn actors_rejects_bad_series_index() {
        let mut query = q(QueryKind::Actors);
        query.actors_config = Some(ActorsConfig {
            series_index: 7,
            day: String::new(),
            offset: 0,
            limit: 10,
        });
        assert!(matches!(
            compile(&query, "phc_t", &src(), None),
            Err(CompileError::Validation(_))
        ));

        query.actors_config = None;
        assert!(matches!(
            compile(&query, "phc_t", &src(), None),
            Err(CompileError::Validation(_))
        ));
    }

    /// The clicked column narrows the drill-down, and the day is user input, so
    /// it binds like everything else.
    #[test]
    fn actors_day_narrows_and_binds() {
        let mut query = q(QueryKind::Actors);
        query.actors_config = Some(ActorsConfig {
            series_index: 0,
            day: "2026-08-04".into(),
            offset: 0,
            limit: 10,
        });
        let compiled = compile(&query, "phc_t", &src(), None).unwrap();
        assert!(
            texts(&compiled).iter().any(|p| p == "2026-08-04"),
            "day not bound"
        );
        assert!(
            !compiled.sql.contains("2026-08-04"),
            "day interpolated into SQL"
        );
    }

    /// IR validation runs before compilation, so a malformed query never
    /// reaches SQL generation.
    #[test]
    fn validation_runs_before_compilation() {
        let mut query = q(QueryKind::Trends);
        query.series = vec![];
        assert!(matches!(
            compile(&query, "phc_t", &src(), None),
            Err(CompileError::Validation(_))
        ));

        let mut query = q(QueryKind::Trends);
        query.series = vec![series("")];
        assert!(matches!(
            compile(&query, "phc_t", &src(), None),
            Err(CompileError::Validation(_))
        ));
    }

    /// A glob containing a quote must not be able to close the string literal
    /// it sits inside — it is structural, so it is escaped rather than bound.
    #[test]
    fn source_expressions_escape_quotes() {
        // Paths are structural, so they are escaped rather than bound — but a
        // quote in a path must still not be able to close the literal it sits in.
        assert_eq!(glob_source("ev'ents/*.parquet"), "'ev''ents/*.parquet'");
        let list = file_list_source(&[
            std::path::PathBuf::from("a/1.parquet"),
            std::path::PathBuf::from("b'/2.parquet"),
        ]);
        assert_eq!(list, "['a/1.parquet', 'b''/2.parquet']");

        let compiled = compile(
            &q(QueryKind::Trends),
            "phc_t",
            &glob_source("ev'ents/*.parquet"),
            None,
        )
        .unwrap();
        assert!(
            compiled.sql.contains("ev''ents/*.parquet"),
            "glob quote not escaped"
        );
    }

    /// The compiler emits two placeholder spellings — `$P` (rewritten to `?` by
    /// `finalize_sql`) and DuckDB's numbered `$1`. Both consume one entry from
    /// the same parameter list, so count them together.
    fn placeholder_count(sql: &str) -> usize {
        let numbered = sql
            .match_indices('$')
            .filter(|(i, _)| {
                sql[i + 1..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
            })
            .count();
        sql.matches("$P").count() + numbered
    }

    /// `finalize_sql` substitutes $P positionally, so the number of bound
    /// parameters must equal the number of placeholders — for every kind, with
    /// and without filters. Ordered funnels got this wrong: each step CTE emits
    /// a token placeholder before its filter placeholders, but the filters were
    /// bound once up front, so everything shifted by one and DuckDB 500'd.
    #[test]
    fn param_count_matches_placeholders() {
        let filtered = PropertyGroup {
            op: GroupOp::And,
            values: vec![GroupOrFilter::Filter(Filter {
                source: FilterSource::Event,
                key: "browser".into(),
                operator: FilterOperator::Exact,
                value: serde_json::json!("Chrome"),
            })],
        };

        for with_filter in [false, true] {
            for order in [FunnelOrder::Ordered, FunnelOrder::Unordered] {
                for steps in 1..=3usize {
                    let mut query = q(QueryKind::Funnels);
                    query.series = (0..steps).map(|i| series(&format!("step{i}"))).collect();
                    query.funnel_config = Some(FunnelConfig {
                        order_type: order,
                        conversion_window_seconds: None,
                        exclusions: vec![],
                        attribution: FunnelAttribution::default(),
                    });
                    if with_filter {
                        query.filters = filtered.clone();
                    }
                    let c = compile(&query, "phc_t", &src(), None).unwrap();
                    assert_eq!(
                        placeholder_count(&c.sql),
                        c.params.len(),
                        "funnels order={order:?} steps={steps} filter={with_filter}",
                    );
                }
            }
        }

        // And the simple kinds, which share the same substitution.
        for kind in [QueryKind::Trends, QueryKind::Actors] {
            let label = format!("{kind:?}");
            let mut query = q(kind);
            query.filters = filtered.clone();
            query.actors_config = Some(ActorsConfig {
                series_index: 0,
                day: "2026-08-04".into(),
                offset: 0,
                limit: 10,
            });
            let c = compile(&query, "phc_t", &src(), None).unwrap();
            assert_eq!(placeholder_count(&c.sql), c.params.len(), "{label}");
        }
    }

    /// Every interval maps to a distinct bucket expression; a collision would
    /// silently answer "by day" for a "by week" question.
    #[test]
    fn intervals_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for interval in [
            Interval::Hour,
            Interval::Day,
            Interval::Week,
            Interval::Month,
        ] {
            assert!(
                seen.insert(interval_sql(interval)),
                "{interval:?} duplicates another interval"
            );
        }
    }
}
