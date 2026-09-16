# tree-regions

Regions of a source file found from the tree-sitter parse tree: today, the
`#[cfg(test)]`-attributed items of a Rust file and the file's parse errors.
Shared by `invariant-audit` (drops violation sites inside test regions) and
`code_index` (classifies definitions inside them as test chunks). Grammars are
compiled in via the `tree-sitter` crate, so there is no runtime grammar load.
