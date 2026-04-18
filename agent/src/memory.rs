use anyhow::{bail, Result};
use chrono::Utc;
use clap::{Args, Subcommand};
use rusqlite::{params, Connection};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Args)]
pub struct MemoryCmd {
    #[command(subcommand)]
    pub action: MemoryAction,
}

#[derive(Subcommand)]
pub enum MemoryAction {
    /// Add a new memory
    Add(AddArgs),
    /// Update an existing memory
    Update(UpdateArgs),
    /// Full-text search memories
    Search(SearchArgs),
    /// Most recently updated memories
    Recent(RecentArgs),
    /// List memories filtered by type/tag/cwd
    List(ListArgs),
    /// Output relevant memories for session start (hook use)
    Context(ContextArgs),
    /// Export all active memories as markdown
    Export,
    /// Import existing markdown memory files into the database
    Migrate(MigrateArgs),
}

#[derive(Args)]
pub struct AddArgs {
    #[arg(long)]
    pub r#type: String,
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub description: String,
    #[arg(long)]
    pub content: String,
    #[arg(long, default_value = "")]
    pub tags: String,
    #[arg(long, default_value = "")]
    pub cwd: String,
    #[arg(long, default_value = "curated")]
    pub source: String,
}

#[derive(Args)]
pub struct UpdateArgs {
    pub id: String,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long)]
    pub content: Option<String>,
    #[arg(long)]
    pub tags: Option<String>,
    #[arg(long)]
    pub active: Option<bool>,
}

#[derive(Args)]
pub struct SearchArgs {
    pub query: String,
    #[arg(long)]
    pub r#type: Option<String>,
    #[arg(long, default_value = "10")]
    pub limit: usize,
}

#[derive(Args)]
pub struct RecentArgs {
    #[arg(long, default_value = "10")]
    pub n: usize,
    #[arg(long)]
    pub r#type: Option<String>,
}

#[derive(Args)]
pub struct ListArgs {
    #[arg(long)]
    pub r#type: Option<String>,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long, default_value = "50")]
    pub limit: usize,
}

#[derive(Args)]
pub struct ContextArgs {
    #[arg(long, default_value = "")]
    pub cwd: String,
    /// Limit per category
    #[arg(long, default_value = "5")]
    pub limit: usize,
}

#[derive(Args)]
pub struct MigrateArgs {
    /// Directory containing markdown memory files
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Dry run — print what would be imported without writing
    #[arg(long)]
    pub dry_run: bool,
}

struct Memory {
    id: String,
    type_: String,
    name: String,
    description: String,
    content: String,
    source: String,
    tags: String,
    cwd: String,
    is_active: bool,
    created_at: i64,
    updated_at: i64,
}

fn short_id() -> String {
    Uuid::new_v4().to_string()[..8].to_string()
}

fn now() -> i64 {
    Utc::now().timestamp()
}

pub fn run(conn: Connection, cmd: MemoryCmd) -> Result<()> {
    match cmd.action {
        MemoryAction::Add(args) => add(&conn, args),
        MemoryAction::Update(args) => update(&conn, args),
        MemoryAction::Search(args) => search(&conn, args),
        MemoryAction::Recent(args) => recent(&conn, args),
        MemoryAction::List(args) => list(&conn, args),
        MemoryAction::Context(args) => context(&conn, args),
        MemoryAction::Export => export(&conn),
        MemoryAction::Migrate(args) => migrate(&conn, args),
    }
}

fn add(conn: &Connection, args: AddArgs) -> Result<()> {
    let valid_types = ["user", "feedback", "project", "reference"];
    if !valid_types.contains(&args.r#type.as_str()) {
        bail!("type must be one of: {}", valid_types.join(", "));
    }
    let id = short_id();
    let ts = now();
    conn.execute(
        "INSERT INTO memories (id, type, name, description, content, source, tags, cwd, is_active, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?9)",
        params![id, args.r#type, args.name, args.description, args.content, args.source, args.tags, args.cwd, ts],
    )?;
    println!("{id}");
    Ok(())
}

fn update(conn: &Connection, args: UpdateArgs) -> Result<()> {
    let ts = now();
    let mut updated = 0;

    if let Some(name) = args.name {
        updated += conn.execute("UPDATE memories SET name=?1, updated_at=?2 WHERE id=?3", params![name, ts, args.id])?;
    }
    if let Some(desc) = args.description {
        updated += conn.execute("UPDATE memories SET description=?1, updated_at=?2 WHERE id=?3", params![desc, ts, args.id])?;
    }
    if let Some(content) = args.content {
        updated += conn.execute("UPDATE memories SET content=?1, updated_at=?2 WHERE id=?3", params![content, ts, args.id])?;
    }
    if let Some(tags) = args.tags {
        updated += conn.execute("UPDATE memories SET tags=?1, updated_at=?2 WHERE id=?3", params![tags, ts, args.id])?;
    }
    if let Some(active) = args.active {
        updated += conn.execute("UPDATE memories SET is_active=?1, updated_at=?2 WHERE id=?3", params![active as i64, ts, args.id])?;
    }

    if updated == 0 {
        bail!("no memory found with id={}", args.id);
    }
    Ok(())
}

fn search(conn: &Connection, args: SearchArgs) -> Result<()> {
    let type_clause = if let Some(ref t) = args.r#type {
        format!("AND m.type = '{t}'")
    } else {
        String::new()
    };

    let sql = format!(
        "SELECT m.id, m.type, m.name, m.description, m.content, m.tags, m.updated_at
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ?1 AND m.is_active = 1 {type_clause}
         ORDER BY rank
         LIMIT ?2"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![args.query, args.limit as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, i64>(6)?,
        ))
    })?;

    for row in rows {
        let (id, type_, name, desc, content, tags, updated_at) = row?;
        let ts = chrono::DateTime::from_timestamp(updated_at, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        println!("[{id}] ({type_}) {name} — {desc}  [{ts}]");
        if !tags.is_empty() {
            println!("  tags: {tags}");
        }
        println!("{}", indent(&content, "  "));
        println!();
    }
    Ok(())
}

fn recent(conn: &Connection, args: RecentArgs) -> Result<()> {
    let type_clause = if let Some(ref t) = args.r#type {
        format!("AND type = '{t}'")
    } else {
        String::new()
    };
    let sql = format!(
        "SELECT id, type, name, description, updated_at FROM memories
         WHERE is_active = 1 {type_clause}
         ORDER BY updated_at DESC LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([args.n as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    for row in rows {
        let (id, type_, name, desc, updated_at) = row?;
        let ts = chrono::DateTime::from_timestamp(updated_at, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        println!("[{id}] ({type_}) {name} — {desc}  [{ts}]");
    }
    Ok(())
}

fn list(conn: &Connection, args: ListArgs) -> Result<()> {
    let mut clauses = vec!["is_active = 1".to_string()];
    if let Some(ref t) = args.r#type {
        clauses.push(format!("type = '{t}'"));
    }
    if let Some(ref cwd) = args.cwd {
        clauses.push(format!("cwd LIKE '%{cwd}%'"));
    }
    if let Some(ref tag) = args.tag {
        clauses.push(format!("tags LIKE '%{tag}%'"));
    }
    let where_ = clauses.join(" AND ");
    let sql = format!(
        "SELECT id, type, name, description, tags, updated_at FROM memories
         WHERE {where_} ORDER BY updated_at DESC LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([args.limit as i64], |r| {
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
        let (id, type_, name, desc, tags, updated_at) = row?;
        let ts = chrono::DateTime::from_timestamp(updated_at, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        let tag_str = if tags.is_empty() { String::new() } else { format!("  [{}]", tags) };
        println!("[{id}] ({type_}) {name}{tag_str} — {desc}  [{ts}]");
    }
    Ok(())
}

fn context(conn: &Connection, args: ContextArgs) -> Result<()> {
    // feedback: always inject all active ones (they're behavioral rules)
    let feedback = query_by_type(conn, "feedback", 20)?;
    // user: always inject
    let user = query_by_type(conn, "user", 5)?;
    // project: recent ones relevant to cwd
    let project = query_recent_by_type_cwd(conn, "project", &args.cwd, args.limit)?;
    // reference: cwd-relevant
    let reference = query_recent_by_type_cwd(conn, "reference", &args.cwd, 3)?;

    if feedback.is_empty() && user.is_empty() && project.is_empty() && reference.is_empty() {
        return Ok(());
    }

    println!("## Active Memory Context\n");

    if !feedback.is_empty() {
        println!("### Behavioral Rules (Feedback)\n");
        for m in &feedback {
            println!("**{}**: {}", m.name, m.description);
            println!("{}\n", m.content);
        }
    }

    if !user.is_empty() {
        println!("### User Profile\n");
        for m in &user {
            println!("{}\n", m.content);
        }
    }

    if !project.is_empty() {
        println!("### Project Context\n");
        for m in &project {
            println!("**{}**: {}", m.name, m.description);
            println!("{}\n", m.content);
        }
    }

    if !reference.is_empty() {
        println!("### References\n");
        for m in &reference {
            println!("**{}**: {}\n", m.name, m.content);
        }
    }

    Ok(())
}

fn query_by_type(conn: &Connection, type_: &str, limit: usize) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT id, type, name, description, content, source, tags, cwd, is_active, created_at, updated_at
         FROM memories WHERE type = ?1 AND is_active = 1 ORDER BY updated_at DESC LIMIT ?2",
    )?;
    let mapped = stmt.query_map(params![type_, limit as i64], row_to_memory)?;
    collect_memories(mapped)
}

fn query_recent_by_type_cwd(conn: &Connection, type_: &str, cwd: &str, limit: usize) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT id, type, name, description, content, source, tags, cwd, is_active, created_at, updated_at
         FROM memories WHERE type = ?1 AND is_active = 1
         ORDER BY (CASE WHEN cwd != '' AND ?2 LIKE '%' || cwd || '%' THEN 1 ELSE 2 END), updated_at DESC
         LIMIT ?3",
    )?;
    let mapped = stmt.query_map(params![type_, cwd, limit as i64], row_to_memory)?;
    collect_memories(mapped)
}

fn row_to_memory(r: &rusqlite::Row) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: r.get(0)?,
        type_: r.get(1)?,
        name: r.get(2)?,
        description: r.get(3)?,
        content: r.get(4)?,
        source: r.get(5)?,
        tags: r.get(6)?,
        cwd: r.get(7)?,
        is_active: r.get::<_, i64>(8)? != 0,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

fn collect_memories<I>(iter: I) -> Result<Vec<Memory>>
where
    I: Iterator<Item = rusqlite::Result<Memory>>,
{
    iter.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn export(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, type, name, description, content, tags FROM memories
         WHERE is_active = 1 ORDER BY type, updated_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    for row in rows {
        let (id, type_, name, desc, content, tags) = row?;
        println!("---");
        println!("id: {id}");
        println!("name: {name}");
        println!("description: {desc}");
        println!("type: {type_}");
        if !tags.is_empty() {
            println!("tags: {tags}");
        }
        println!("---\n");
        println!("{content}\n");
    }
    Ok(())
}

fn migrate(conn: &Connection, args: MigrateArgs) -> Result<()> {
    let dir = args.dir.unwrap_or_else(|| {
        let mut p = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
        p.push(".claude-personal/projects/-home-tcovert/memory");
        p
    });

    let entries: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().extension().map(|x| x == "md").unwrap_or(false)
                && e.file_name() != "MEMORY.md"
        })
        .collect();

    let mut imported = 0;
    let mut skipped = 0;

    for entry in entries {
        let path = entry.path();
        let raw = std::fs::read_to_string(&path)?;
        match parse_frontmatter(&raw) {
            Some((type_, name, description, content)) => {
                let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                let id = stem.as_ref();
                let ts = now();

                if args.dry_run {
                    println!("would import: [{id}] ({type_}) {name}");
                    imported += 1;
                    continue;
                }

                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
                    [id],
                    |r| r.get(0),
                )?;

                if exists {
                    skipped += 1;
                    continue;
                }

                conn.execute(
                    "INSERT INTO memories (id, type, name, description, content, source, tags, cwd, is_active, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'curated', '', '', 1, ?6, ?6)",
                    params![id, type_, name, description, content, ts],
                )?;
                println!("imported: [{id}] ({type_}) {name}");
                imported += 1;
            }
            None => {
                eprintln!("skipping (no valid frontmatter): {}", path.display());
                skipped += 1;
            }
        }
    }

    eprintln!("done: {imported} imported, {skipped} skipped");
    Ok(())
}

fn parse_frontmatter(raw: &str) -> Option<(String, String, String, String)> {
    let raw = raw.trim_start();
    let rest = raw.strip_prefix("---")?.trim_start_matches('\n');
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    // skip "\n---" (4 bytes) then any trailing newline on the closing delimiter line
    let after_close = rest[end + 4..].trim_start_matches('\n');
    let body = after_close;

    let mut type_ = String::new();
    let mut name = String::new();
    let mut description = String::new();

    for line in front.lines() {
        if let Some(v) = line.strip_prefix("type:") {
            type_ = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("name:") {
            name = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("description:") {
            description = v.trim().to_string();
        }
    }

    if type_.is_empty() || name.is_empty() || description.is_empty() {
        return None;
    }

    Some((type_, name, description, body.to_string()))
}

fn indent(s: &str, prefix: &str) -> String {
    s.lines().map(|l| format!("{prefix}{l}")).collect::<Vec<_>>().join("\n")
}
