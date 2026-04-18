use anyhow::{bail, Result};
use chrono::Utc;
use clap::{Args, Subcommand};
use rusqlite::{params, types::ToSql, Connection};
use uuid::Uuid;

#[derive(Args)]
pub struct TaskCmd {
    #[command(subcommand)]
    pub action: TaskAction,
}

#[derive(Subcommand)]
pub enum TaskAction {
    /// Create a new task
    Create(CreateArgs),
    /// Update task status or result
    Update(UpdateArgs),
    /// List tasks
    List(ListArgs),
    /// Show full task details
    Show(ShowArgs),
    /// List in_progress and suspended tasks for resumption
    Resume,
}

#[derive(Args)]
pub struct CreateArgs {
    #[arg(long)]
    pub objective: String,
    #[arg(long, default_value = "research")]
    pub task_type: String,
    #[arg(long, default_value = "")]
    pub cwd: String,
    #[arg(long)]
    pub parent_id: Option<String>,
}

#[derive(Args)]
pub struct UpdateArgs {
    pub id: String,
    #[arg(long)]
    pub status: Option<String>,
    #[arg(long)]
    pub result: Option<String>,
    #[arg(long)]
    pub completion_id: Option<String>,
}

#[derive(Args)]
pub struct ListArgs {
    #[arg(long)]
    pub status: Option<String>,
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long, default_value = "20")]
    pub limit: usize,
    #[arg(long, default_value = "7")]
    pub days: u32,
}

#[derive(Args)]
pub struct ShowArgs {
    pub id: String,
}

const VALID_STATUSES: &[&str] = &["pending", "in_progress", "completed", "failed", "suspended"];

fn short_id() -> String {
    Uuid::new_v4().to_string()[..8].to_string()
}

fn now() -> i64 {
    Utc::now().timestamp()
}

pub fn run(conn: Connection, cmd: TaskCmd) -> Result<()> {
    match cmd.action {
        TaskAction::Create(args) => create(&conn, args),
        TaskAction::Update(args) => update(&conn, args),
        TaskAction::List(args) => list(&conn, args),
        TaskAction::Show(args) => show(&conn, args),
        TaskAction::Resume => resume(&conn),
    }
}

fn create(conn: &Connection, args: CreateArgs) -> Result<()> {
    let id = short_id();
    let ts = now();
    conn.execute(
        "INSERT INTO tasks (id, status, objective, task_type, cwd, parent_task_id, created_at, updated_at)
         VALUES (?1, 'pending', ?2, ?3, ?4, ?5, ?6, ?6)",
        params![id, args.objective, args.task_type, args.cwd, args.parent_id, ts],
    )?;
    println!("{id}");
    Ok(())
}

fn update(conn: &Connection, args: UpdateArgs) -> Result<()> {
    let ts = now();

    if let Some(ref status) = args.status {
        if !VALID_STATUSES.contains(&status.as_str()) {
            bail!("status must be one of: {}", VALID_STATUSES.join(", "));
        }
        let n = conn.execute(
            "UPDATE tasks SET status=?1, updated_at=?2 WHERE id=?3",
            params![status, ts, args.id],
        )?;
        if n == 0 {
            bail!("no task with id={}", args.id);
        }
    }

    if let Some(ref result) = args.result {
        let n = conn.execute(
            "UPDATE tasks SET result=?1, updated_at=?2 WHERE id=?3",
            params![result, ts, args.id],
        )?;
        if n == 0 {
            bail!("no task with id={}", args.id);
        }
    }

    if let Some(ref cid) = args.completion_id {
        let n = conn.execute(
            "UPDATE tasks SET completion_id=?1, updated_at=?2 WHERE id=?3",
            params![cid, ts, args.id],
        )?;
        if n == 0 {
            bail!("no task with id={}", args.id);
        }
    }

    Ok(())
}

fn list(conn: &Connection, args: ListArgs) -> Result<()> {
    let cutoff = now() - (args.days as i64 * 86400);
    let mut clauses = vec!["updated_at >= ?".to_string()];
    let mut param_boxes: Vec<Box<dyn ToSql>> = vec![Box::new(cutoff)];
    if let Some(ref s) = args.status {
        clauses.push("status = ?".to_string());
        param_boxes.push(Box::new(s.clone()));
    }
    if let Some(ref cwd) = args.cwd {
        clauses.push("cwd LIKE ?".to_string());
        param_boxes.push(Box::new(format!("%{cwd}%")));
    }
    param_boxes.push(Box::new(args.limit as i64));
    let where_ = clauses.join(" AND ");
    let sql = format!(
        "SELECT id, status, task_type, objective, cwd, updated_at FROM tasks
         WHERE {where_} ORDER BY updated_at DESC LIMIT ?"
    );
    let params: Vec<&dyn ToSql> = param_boxes.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params.as_slice(), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    for row in rows {
        let (id, status, task_type, objective, cwd, updated_at) = row?;
        let ts = chrono::DateTime::from_timestamp(updated_at, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        let cwd_str = if cwd.is_empty() { String::new() } else { format!("  cwd: {cwd}") };
        println!("[{id}] {status:12} ({task_type}) {ts}");
        println!("  {objective}{cwd_str}");
    }
    Ok(())
}

fn show(conn: &Connection, args: ShowArgs) -> Result<()> {
    let row = conn.query_row(
        "SELECT id, status, task_type, objective, cwd, parent_task_id, completion_id, result, created_at, updated_at
         FROM tasks WHERE id = ?1",
        [&args.id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, i64>(9)?,
            ))
        },
    );

    match row {
        Err(rusqlite::Error::QueryReturnedNoRows) => bail!("no task with id={}", args.id),
        Err(e) => return Err(e.into()),
        Ok((id, status, task_type, objective, cwd, parent_id, completion_id, result, created_at, updated_at)) => {
            let fmt = |ts: i64| {
                chrono::DateTime::from_timestamp(ts, 0)
                    .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default()
            };
            println!("id:          {id}");
            println!("status:      {status}");
            println!("type:        {task_type}");
            println!("objective:   {objective}");
            if !cwd.is_empty() { println!("cwd:         {cwd}"); }
            if let Some(p) = parent_id { println!("parent:      {p}"); }
            if let Some(c) = completion_id { println!("completion:  {c}"); }
            println!("created:     {}", fmt(created_at));
            println!("updated:     {}", fmt(updated_at));
            if let Some(r) = result {
                println!("result:");
                println!("{r}");
            }
        }
    }
    Ok(())
}

fn resume(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, status, task_type, objective, cwd, updated_at FROM tasks
         WHERE status IN ('in_progress', 'suspended') ORDER BY updated_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    let mut any = false;
    for row in rows {
        let (id, status, task_type, objective, cwd, updated_at) = row?;
        let ts = chrono::DateTime::from_timestamp(updated_at, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        println!("[{id}] {status:12} ({task_type}) {ts}");
        println!("  {objective}");
        if !cwd.is_empty() { println!("  cwd: {cwd}"); }
        any = true;
    }
    if !any {
        println!("(no resumable tasks)");
    }
    Ok(())
}
