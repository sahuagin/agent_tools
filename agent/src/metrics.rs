use anyhow::Result;
use chrono::Utc;
use clap::{Args, Subcommand};
use rusqlite::{params, types::ToSql, Connection};
use uuid::Uuid;

#[derive(Args)]
pub struct MetricsCmd {
    #[command(subcommand)]
    pub action: MetricsAction,
}

#[derive(Subcommand)]
pub enum MetricsAction {
    /// Record a completion (one per orchestrate run or pi session)
    RecordCompletion(RecordCompletionArgs),
    /// Record token/cost usage for a completion
    RecordUsage(RecordUsageArgs),
    /// Show aggregated metrics report
    Report(ReportArgs),
    /// List recent completions
    List(ListArgs),
}

#[derive(Args)]
pub struct RecordCompletionArgs {
    #[arg(long)]
    pub task_id: Option<String>,
    #[arg(long)]
    pub session_id: Option<String>,
    #[arg(long, default_value = "")]
    pub model: String,
    #[arg(long, default_value = "")]
    pub provider: String,
    #[arg(long, default_value = "research")]
    pub task_type: String,
    #[arg(long, default_value = "")]
    pub objective: String,
    #[arg(long, default_value = "")]
    pub cwd: String,
    #[arg(long, default_value = "")]
    pub status: String,
    #[arg(long, default_value = "0.0")]
    pub confidence: f64,
    #[arg(long, default_value = "0")]
    pub tool_calls: i64,
    #[arg(long, default_value = "0")]
    pub wall_ms: i64,
}

#[derive(Args)]
pub struct RecordUsageArgs {
    #[arg(long)]
    pub completion_id: String,
    #[arg(long, default_value = "0")]
    pub input_tokens: i64,
    #[arg(long, default_value = "0")]
    pub output_tokens: i64,
    #[arg(long, default_value = "0")]
    pub cache_read: i64,
    #[arg(long, default_value = "0")]
    pub cache_write: i64,
    #[arg(long, default_value = "0.0")]
    pub cost_usd: f64,
}

#[derive(Args)]
pub struct ReportArgs {
    #[arg(long, default_value = "30")]
    pub days: u32,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub task_type: Option<String>,
}

#[derive(Args)]
pub struct ListArgs {
    #[arg(long, default_value = "10")]
    pub limit: usize,
    #[arg(long, default_value = "7")]
    pub days: u32,
}

fn short_id() -> String {
    Uuid::new_v4().to_string()[..8].to_string()
}

fn now() -> i64 {
    Utc::now().timestamp()
}

pub fn run(conn: Connection, cmd: MetricsCmd) -> Result<()> {
    match cmd.action {
        MetricsAction::RecordCompletion(args) => record_completion(&conn, args),
        MetricsAction::RecordUsage(args) => record_usage(&conn, args),
        MetricsAction::Report(args) => report(&conn, args),
        MetricsAction::List(args) => list(&conn, args),
    }
}

fn record_completion(conn: &Connection, args: RecordCompletionArgs) -> Result<()> {
    let id = short_id();
    let ts = now();
    conn.execute(
        "INSERT INTO completions
         (id, task_id, session_id, model, provider, task_type, objective, cwd,
          status, confidence, tool_calls, wall_ms, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            id,
            args.task_id,
            args.session_id,
            args.model,
            args.provider,
            args.task_type,
            args.objective,
            args.cwd,
            args.status,
            args.confidence,
            args.tool_calls,
            args.wall_ms,
            ts
        ],
    )?;
    println!("{id}");
    Ok(())
}

fn record_usage(conn: &Connection, args: RecordUsageArgs) -> Result<()> {
    let ts = now();
    conn.execute(
        "INSERT INTO usage_events
         (completion_id, input_tokens, output_tokens, cache_read, cache_write, cost_usd, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            args.completion_id,
            args.input_tokens,
            args.output_tokens,
            args.cache_read,
            args.cache_write,
            args.cost_usd,
            ts
        ],
    )?;
    Ok(())
}

fn report(conn: &Connection, args: ReportArgs) -> Result<()> {
    let cutoff = now() - (args.days as i64 * 86400);

    let mut clauses = vec!["c.created_at >= ?".to_string()];
    let mut param_boxes: Vec<Box<dyn ToSql>> = vec![Box::new(cutoff)];
    if let Some(ref m) = args.model {
        clauses.push("c.model = ?".to_string());
        param_boxes.push(Box::new(m.clone()));
    }
    if let Some(ref t) = args.task_type {
        clauses.push("c.task_type = ?".to_string());
        param_boxes.push(Box::new(t.clone()));
    }
    let where_ = clauses.join(" AND ");
    let params: Vec<&dyn ToSql> = param_boxes.iter().map(|b| b.as_ref()).collect();

    let sql = format!(
        "WITH u_agg AS (
            SELECT completion_id,
                   SUM(cost_usd)                        AS cost_usd,
                   SUM(input_tokens + output_tokens)     AS total_tokens,
                   SUM(cache_read)                       AS cache_read
            FROM usage_events GROUP BY completion_id
         )
         SELECT
            c.model,
            c.provider,
            c.task_type,
            COUNT(*) as runs,
            SUM(CASE WHEN c.status = 'completed' THEN 1 ELSE 0 END) as successes,
            AVG(c.wall_ms) as avg_wall_ms,
            AVG(c.tool_calls) as avg_tools,
            SUM(u.cost_usd) as total_cost,
            AVG(u.total_tokens) as avg_tokens,
            AVG(CASE WHEN u.total_tokens > 0
                THEN CAST(u.cache_read AS REAL) / u.total_tokens
                ELSE NULL END) as avg_cache_ratio
         FROM completions c
         LEFT JOIN u_agg u ON u.completion_id = c.id
         WHERE {where_}
         GROUP BY c.model, c.provider, c.task_type
         ORDER BY total_cost DESC"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params.as_slice(), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Option<f64>>(5)?,
            r.get::<_, Option<f64>>(6)?,
            r.get::<_, Option<f64>>(7)?,
            r.get::<_, Option<f64>>(8)?,
            r.get::<_, Option<f64>>(9)?,
        ))
    })?;

    println!(
        "{:<35} {:<10} {:>5} {:>5} {:>8} {:>7} {:>10} {:>8} {:>7}",
        "model/task_type",
        "provider",
        "runs",
        "ok%",
        "avg_ms",
        "tools",
        "cost_usd",
        "tokens",
        "cache%"
    );
    println!("{}", "-".repeat(100));

    for row in rows {
        let (
            model,
            provider,
            task_type,
            runs,
            successes,
            avg_ms,
            avg_tools,
            total_cost,
            avg_tokens,
            cache_ratio,
        ) = row?;
        let ok_pct = if runs > 0 { successes * 100 / runs } else { 0 };
        let label = format!("{model} / {task_type}");
        println!(
            "{:<35} {:<10} {:>5} {:>4}% {:>8} {:>7.1} {:>10.6} {:>8.0} {:>6.0}%",
            label,
            provider,
            runs,
            ok_pct,
            avg_ms.map(|v| format!("{:.0}", v)).unwrap_or_default(),
            avg_tools.unwrap_or(0.0),
            total_cost.unwrap_or(0.0),
            avg_tokens.unwrap_or(0.0),
            cache_ratio.unwrap_or(0.0) * 100.0,
        );
    }
    Ok(())
}

fn list(conn: &Connection, args: ListArgs) -> Result<()> {
    let cutoff = now() - (args.days as i64 * 86400);
    let mut stmt = conn.prepare(
        "SELECT c.id, c.model, c.task_type, c.status, c.wall_ms, u.cost_usd, c.objective, c.created_at
         FROM completions c
         LEFT JOIN usage_events u ON u.completion_id = c.id
         WHERE c.created_at >= ?1
         ORDER BY c.created_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![cutoff, args.limit as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, Option<f64>>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, i64>(7)?,
        ))
    })?;
    for row in rows {
        let (id, model, task_type, status, wall_ms, cost, objective, created_at) = row?;
        let ts = chrono::DateTime::from_timestamp(created_at, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        let ms_str = wall_ms.map(|v| format!("{v}ms")).unwrap_or_default();
        let cost_str = cost.map(|v| format!("${v:.4}")).unwrap_or_default();
        println!("[{id}] {ts}  {status:12} {model} ({task_type})  {ms_str}  {cost_str}");
        let obj_short: String = objective.chars().take(80).collect();
        println!("  {obj_short}");
    }
    Ok(())
}
