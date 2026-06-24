//! Read/write classification for the `agent` CLI's leaf verbs — the single
//! source of truth shared by the client (forward routing's reads-local /
//! writes-fail-loud policy) and the server (`agent-mcp`'s write-gating + its
//! `validate_read_classification` drift guard). Keyed on the normalized
//! snake_case leaf/tool name (e.g. `memory_recall`, `task_create`).

/// Leaf names (normalized snake_case) that ONLY read. Everything else is treated
/// as a write. `agent-mcp`'s `validate_read_classification` asserts at startup
/// that every entry maps to a live leaf, so this hand-maintained list cannot
/// silently drift from the CLI.
pub const READ_TOOLS: &[&str] = &[
    "memory_show",
    "memory_events",
    "memory_patch_log",
    "memory_diff",
    "memory_search",
    "memory_recent",
    "memory_list",
    "memory_context",
    "memory_context_stats",
    "memory_recall",
    "memory_recall_stats",
    "memory_resolve",
    "memory_kernel_show",
    "memory_export",
    "task_list",
    "task_show",
    "task_resume",
    "metrics_report",
    "metrics_list",
    "db_path",
];

/// True if the named leaf/tool mutates state — everything not in [`READ_TOOLS`].
/// Unknown names default to write (fail-closed): the unreachable-endpoint policy
/// then refuses them rather than risk a silent local write.
pub fn is_write(tool: &str) -> bool {
    !READ_TOOLS.contains(&tool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_classified() {
        assert!(!is_write("memory_recall"));
        assert!(!is_write("memory_list"));
        assert!(!is_write("db_path"));
        assert!(is_write("memory_add"));
        assert!(is_write("task_create"));
        // Unknown → write (fail-closed).
        assert!(is_write("memory_brand_new_verb"));
    }
}
