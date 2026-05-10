//! Walk tree-sitter `Query` matches, pair `@name` + `@definition.X`
//! captures into `Chunk` records, and pair `@name` + `@reference.X`
//! captures into `RawReference` records.
//!
//! Definition span dedup (impl-method-vs-function in tree-sitter-rust)
//! is handled by `kind_precedence`; references are NOT deduped — a
//! single function can legitimately reference the same target multiple
//! times, but each reference is at a different byte position so they
//! don't collide on (start_byte, end_byte) keys.

use std::collections::HashMap;
use std::path::Path;

use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

use crate::{Chunk, ChunkKind};

use super::{
    build_chunk, chunk_kind_from_tag, edge_kind_from_reference_tag, ExtractResult,
    RawReference,
};

/// Run the query over `root` and produce both definition chunks and
/// raw (un-resolved) references. References are post-attributed to
/// their containing chunk via byte-range containment.
pub(super) fn collect_chunks_and_references(
    query: &Query,
    cursor: &mut QueryCursor,
    root: Node<'_>,
    source: &[u8],
    file: &Path,
) -> ExtractResult {
    let capture_names = query.capture_names();

    // Two parallel collectors: definition chunks (with span dedup) and
    // raw references (with their byte position retained for parent-walk).
    let mut by_span: HashMap<(usize, usize), (Chunk, u8)> = HashMap::new();
    let mut raw_refs: Vec<(RawReference, usize)> = Vec::new();

    let mut matches = cursor.matches(query, root, source);
    while let Some(m) = matches.next() {
        let mut name_node: Option<Node<'_>> = None;
        let mut span_node: Option<Node<'_>> = None;
        let mut def_tag: Option<&str> = None;
        let mut ref_tag: Option<&str> = None;

        for cap in m.captures {
            let capname = capture_names[cap.index as usize];
            if capname == "name" {
                name_node = Some(cap.node);
            } else if let Some(rest) = capname.strip_prefix("definition.") {
                span_node = Some(cap.node);
                def_tag = Some(rest);
            } else if let Some(rest) = capname.strip_prefix("reference.") {
                span_node = Some(cap.node);
                ref_tag = Some(rest);
            }
        }

        let Some(name) = name_node else { continue };
        let Some(span) = span_node else { continue };
        let Ok(name_text) = name.utf8_text(source) else { continue };

        if let Some(tag) = def_tag {
            // Definition: build a Chunk, dedup by span, prefer specific kinds.
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
            let prec = kind_precedence(kind);
            match by_span.entry(key) {
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert((chunk, prec));
                }
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    if prec > o.get().1 {
                        o.insert((chunk, prec));
                    }
                }
            }
        } else if let Some(tag) = ref_tag {
            // Reference: keep the byte position so we can attribute it
            // to a containing chunk after definitions are finalized.
            raw_refs.push((
                RawReference {
                    name: name_text.to_string(),
                    kind: edge_kind_from_reference_tag(tag),
                    containing_chunk_idx: None,
                },
                span.start_byte(),
            ));
        }
    }

    // Materialize chunks in document order.
    let mut chunks: Vec<((usize, usize), Chunk)> = by_span
        .into_iter()
        .map(|(key, (chunk, _))| (key, chunk))
        .collect();
    chunks.sort_by_key(|((start, _), _)| *start);

    // Build (start, end, idx) triples so we can attribute references to
    // their innermost containing chunk. We pick the SMALLEST containing
    // span — if a method `inside` lives inside an impl block, the
    // method's body is more specific than the surrounding chunk.
    let span_index: Vec<(usize, usize, usize)> = chunks
        .iter()
        .enumerate()
        .map(|(i, ((s, e), _))| (*s, *e, i))
        .collect();

    let chunk_vec: Vec<Chunk> = chunks.into_iter().map(|(_, c)| c).collect();

    let mut references: Vec<RawReference> = Vec::with_capacity(raw_refs.len());
    for (mut r, ref_start) in raw_refs {
        // Find the chunk with the smallest span containing ref_start.
        let mut best: Option<(usize, usize, usize)> = None;
        for &(s, e, idx) in &span_index {
            if s <= ref_start && ref_start < e {
                let span_size = e - s;
                if best.map(|(_, _, sz)| span_size < sz).unwrap_or(true) {
                    best = Some((idx, s, span_size));
                }
            }
        }
        r.containing_chunk_idx = best.map(|(idx, _, _)| idx);
        references.push(r);
    }

    ExtractResult {
        chunks: chunk_vec,
        references,
    }
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
