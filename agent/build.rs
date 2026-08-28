//! at-g5u: embed build provenance (change id + build time) at compile time.
//!
//! The deployed `~/.local/bin/agent` is a symlink into the checkout's
//! `target/release/agent`, so "deployed" silently tracks whatever was last
//! built there, including uncommitted working-tree edits. Debug vs release
//! builds can also skew minutes apart while testing. This build script bakes
//! the jj change id (or git commit), dirty flag, and build timestamp into
//! env vars that the binary prints via `agent --version` — so "is this
//! binary current with source?" is one command.

use std::process::Command;

fn main() {
    // Re-run if the source tree changes (dirty flag can flip).
    println!("cargo:rerun-if-changed=src");

    // Build timestamp (UTC ISO 8601).
    let build_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Render as UTC ISO 8601 (YYYY-MM-DDTHH:MM:SSZ) — simple civil-from-days.
    let secs = build_time;
    let days = (secs / 86400) as i64;
    let secs_of_day = secs % 86400;
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    let z = if days >= 0 { days + 719468 } else { days + 719468 - 1 };
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (if z >= 0 { z } else { z - 146096 }) - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let build_time_str = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hh, mm, ss);
    println!("cargo:rustc-env=AGENT_BUILD_TIME={build_time_str}");

    // Change id: prefer jj (the repo's VCS), fall back to git.
    let change_id = command_output("jj", &["log", "-r", "@-", "-T", "change_id.short()", "--no-graph"])
        .or_else(|| command_output("git", &["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=AGENT_CHANGE_ID={change_id}");

    // Dirty flag: jj status (working copy has changes) or git status --porcelain.
    let dirty = if let Some(out) = command_output("jj", &["log", "-r", "@", "-T", "description.first_line()", "--no-graph"]) {
        // jj: clean if the description is "(empty)" or "(no description set)".
        out.contains("(empty)") || out.contains("(no description set)")
    } else {
        // git: clean if status --porcelain is empty.
        command_output("git", &["status", "--porcelain"]).map_or(true, |s| s.is_empty())
    };
    println!("cargo:rustc-env=AGENT_DIRTY={}", if dirty { "clean" } else { "dirty" });
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}
