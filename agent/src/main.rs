mod db;
mod memory;
mod metrics;
mod tasks;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agent", about = "Agent memory, tasks, and metrics store")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Memory store: add, update, search, context, migrate
    Memory(memory::MemoryCmd),
    /// Task state machine: create, update, list, resume
    Task(tasks::TaskCmd),
    /// Metrics: record-completion, record-usage, report
    Metrics(metrics::MetricsCmd),
    /// Print path to agent.sqlite
    DbPath,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Command::DbPath = cli.command {
        println!("{}", db::db_path().display());
        return Ok(());
    }

    let conn = db::open()?;

    match cli.command {
        Command::Memory(cmd) => memory::run(conn, cmd),
        Command::Task(cmd) => tasks::run(conn, cmd),
        Command::Metrics(cmd) => metrics::run(conn, cmd),
        Command::DbPath => unreachable!(),
    }
}
