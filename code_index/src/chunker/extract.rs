//! Walk tree-sitter `Query` matches and pair `@name` + `@definition.X`
//! captures into `Chunk` records.

use std::collections::HashMap;
use std::path::Path;

use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

use crate::{Chunk, ChunkKind};

use super::{build_chunk, chunk_kind_from_tag};

/// Iterate matches over `root` against `query` and collect a `Chunk` for
/// each match that contains both a `@name` capture and a `@definition.X`
/// capture. Matches missing either are silently dropped — typically
/// `@reference.X` matches we don't emit at this stage.
///
/// Some tags.scm files (notably tree-sitter-rust) have overlapping
/// patterns where the same span is captured by multiple `@definition.X`
/// tags — e.g. impl-block functions match both `(function_item)` →
/// `function` AND `(declaration_list (function_item))` → `method`.
/// We dedupe by byte span and prefer the more specific kind via
/// `kind_precedence`.
pub(super) fn collect_chunks(
    query: &Query,
    cursor: &mut QueryCursor,
    root: Node<'_>,
    source: &[u8],
    file: &Path,
) -> Vec<Chunk> {
    let capture_names = query.capture_names();
    // Keyed by the byte-range of the @definition.X capture; value is the
    // most-specific Chunk we've seen for that span so far.
    let mut by_span: HashMap<(usize, usize), Chunk> = HashMap::new();

    let mut matches = cursor.matches(query, root, source);
    while let Some(m) = matches.next() {
        let mut name_node: Option<Node<'_>> = None;
        let mut span_node: Option<Node<'_>> = None;
        let mut tag_suffix: Option<&str> = None;

        for cap in m.captures {
            let capname = capture_names[cap.index as usize];
            if capname == "name" {
                name_node = Some(cap.node);
            } else if let Some(rest) = capname.strip_prefix("definition.") {
                span_node = Some(cap.node);
                tag_suffix = Some(rest);
            }
            // @reference.X and other captures are ignored here.
        }

        let (Some(name), Some(span), Some(tag)) = (name_node, span_node, tag_suffix)
        else {
            continue;
        };

        let Ok(name_text) = name.utf8_text(source) else { continue };
        let Ok(span_text) = span.utf8_text(source) else { continue };
        let kind = chunk_kind_from_tag(tag);

        let chunk = build_chunk(
            file,
            name_text,
            span_text,
            span.start_position().row,
            span.end_position().row,
            kind,
        );

        let key = (span.start_byte(), span.end_byte());
        match by_span.entry(key) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(chunk);
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                if kind_precedence(kind) > kind_precedence(o.get().kind) {
                    o.insert(chunk);
                }
            }
        }
    }

    // Emit in document order so callers see chunks roughly top-to-bottom
    // of each file. Stable across runs; useful for diff-style review.
    let mut out: Vec<Chunk> = by_span.into_values().collect();
    out.sort_by_key(|c| c.lines.start);
    out
}

/// Higher value wins when two captures cover the same span. Designed so
/// "method" beats "function" (both can fire on impl-block fns in
/// tree-sitter-rust); other kinds don't currently overlap, but if/when
/// they do, the more-specific tag should be ranked higher.
fn kind_precedence(k: ChunkKind) -> u8 {
    match k {
        ChunkKind::Method => 10,
        ChunkKind::Test => 9,
        ChunkKind::Macro => 8,
        ChunkKind::Class
        | ChunkKind::Struct
        | ChunkKind::Enum
        | ChunkKind::Trait
        | ChunkKind::Interface
        | ChunkKind::Type
        | ChunkKind::Module
        | ChunkKind::Constant
        | ChunkKind::Impl => 7,
        ChunkKind::Function => 5,
        ChunkKind::Other => 0,
    }
}
