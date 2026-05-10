//! AST-aware code chunker driven by vendored `tags.scm` queries.
//!
//! Each supported language has a tree-sitter grammar and a vendored
//! `tags.scm` (under `queries/<lang>/tags.scm`) lifted from the upstream
//! tree-sitter-* repo. The chunker compiles the query once per language
//! at construction time and reuses it for each `extract` call.
//!
//! Tags-format convention (github-tree-sitter-tags / tree-sitter-cli):
//!   `@name`            → name token of the definition (identifier)
//!   `@definition.X`    → entire definition span; X ∈ {function, method,
//!                          class, interface, module, constant, macro, ...}
//!   `@reference.X`     → references; ignored at the chunker stage. Edge
//!                          extraction is a later pass.

use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor};

use crate::{Chunk, ChunkId, ChunkKind, EdgeKind};

mod extract;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SupportedLanguage {
    Rust,
    Python,
    /// TypeScript and (most of) JavaScript. The TypeScript grammar parses
    /// JS as a syntactic subset, so .js/.mjs/.cjs files route here too —
    /// avoiding a separate tree-sitter-javascript dep. The one thing this
    /// can't parse is JSX inside .js files; those need `Tsx`.
    TypeScript,
    /// TSX grammar — TypeScript + JSX. .tsx/.jsx route here.
    Tsx,
}

impl SupportedLanguage {
    /// Map a file extension (without leading dot) to a supported language.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "ts" | "mts" | "cts" | "js" | "mjs" | "cjs" => Some(Self::TypeScript),
            "tsx" | "jsx" => Some(Self::Tsx),
            _ => None,
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }

    pub fn ts_language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }

    pub fn tags_scm(self) -> &'static str {
        match self {
            Self::Rust => include_str!("queries/rust/tags.scm"),
            Self::Python => include_str!("queries/python/tags.scm"),
            // TS and TSX share the same tags.scm — node types are
            // identical between the two grammars; only JSX support differs.
            Self::TypeScript | Self::Tsx => include_str!("queries/typescript/tags.scm"),
        }
    }
}

pub struct Chunker {
    language: SupportedLanguage,
    ts_language: Language,
    query: Query,
}

impl Chunker {
    pub fn for_language(language: SupportedLanguage) -> Result<Self> {
        let ts_language = language.ts_language();
        let query = Query::new(&ts_language, language.tags_scm())
            .with_context(|| format!("compiling tags.scm for {language:?}"))?;
        Ok(Self {
            language,
            ts_language,
            query,
        })
    }

    /// Construct a chunker by inferring the language from the file's
    /// extension. Returns `None` if the extension isn't supported, which
    /// callers should treat as "skip this file" rather than an error.
    pub fn for_path(path: &Path) -> Option<Result<Self>> {
        SupportedLanguage::from_path(path).map(Self::for_language)
    }

    pub fn language(&self) -> SupportedLanguage {
        self.language
    }

    /// Parse `source` and emit a `Chunk` for each `@definition.X` match.
    /// `Chunk.id` is set to `ChunkId(0)` — the caller assigns ids by
    /// upserting into a `Store`.
    pub fn extract(&self, source: &[u8], file: &Path) -> Result<Vec<Chunk>> {
        Ok(self.extract_with_references(source, file)?.chunks)
    }

    /// Parse `source` and emit BOTH `@definition.X` chunks AND
    /// `@reference.X` raw references. Each reference is attributed to
    /// its containing definition via byte-range containment (parent
    /// walk via `Node::start_byte`/`end_byte`). Cross-file resolution
    /// is the caller's job — see `crate::edges`.
    pub fn extract_with_references(
        &self,
        source: &[u8],
        file: &Path,
    ) -> Result<ExtractResult> {
        let mut parser = Parser::new();
        parser
            .set_language(&self.ts_language)
            .context("Parser::set_language")?;
        let tree = parser
            .parse(source, None)
            .context("tree-sitter failed to parse source")?;
        let mut cursor = QueryCursor::new();
        Ok(extract::collect_chunks_and_references(
            &self.query,
            &mut cursor,
            tree.root_node(),
            source,
            file,
        ))
    }
}

/// Output of [`Chunker::extract_with_references`] — all the chunks plus
/// the references they contain.
#[derive(Debug, Clone)]
pub struct ExtractResult {
    pub chunks: Vec<Chunk>,
    pub references: Vec<RawReference>,
}

/// A reference captured by a `@reference.X` tag in a `tags.scm` query,
/// before name resolution. Carries the index of the containing chunk
/// (or `None` if the reference is at file scope, outside any captured
/// definition span).
#[derive(Debug, Clone)]
pub struct RawReference {
    /// The name being referenced (e.g. "into_owned", "FixedBuffer").
    pub name: String,
    /// What kind of edge this would be after resolution.
    pub kind: EdgeKind,
    /// Index into `ExtractResult.chunks`; the chunk whose span contains
    /// this reference. `None` for file-level references.
    pub containing_chunk_idx: Option<usize>,
}

pub(crate) fn edge_kind_from_reference_tag(tag: &str) -> EdgeKind {
    match tag {
        "call" => EdgeKind::Calls,
        "module" => EdgeKind::Imports,
        "implementation" => EdgeKind::Implements,
        // class, macro, type, interface, etc. — collapse to References for v1.
        _ => EdgeKind::References,
    }
}

/// FNV-1a 64-bit hash over arbitrary bytes. Used as the chunk content
/// signature for staleness detection. Deterministic across processes
/// (unlike SipHash with random keys), zero deps, ~5 lines.
pub(crate) fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Map a `@definition.X` tag suffix to a `ChunkKind`.
///
/// Upstream tags.scm conventions collapse some kinds (e.g. tree-sitter-rust
/// tags structs/enums/unions/type-aliases all as `class`). We preserve the
/// less-specific mapping at this stage; refinement based on parent node
/// type is a future enhancement.
pub(crate) fn chunk_kind_from_tag(tag: &str) -> ChunkKind {
    match tag {
        "function" => ChunkKind::Function,
        "method" => ChunkKind::Method,
        "class" => ChunkKind::Class,
        "struct" => ChunkKind::Struct,
        "enum" => ChunkKind::Enum,
        "trait" => ChunkKind::Trait,
        "impl" => ChunkKind::Impl,
        "interface" => ChunkKind::Interface,
        "type" => ChunkKind::Type,
        "module" => ChunkKind::Module,
        "constant" => ChunkKind::Constant,
        "macro" => ChunkKind::Macro,
        "test" => ChunkKind::Test,
        _ => ChunkKind::Other,
    }
}

/// Helper used by `extract::collect_chunks` to construct a `Chunk` once
/// the name and definition spans have been paired from a query match.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_chunk(
    file: &Path,
    name: &str,
    span_text: &str,
    line_start: usize,
    line_end: usize,
    kind: ChunkKind,
) -> Chunk {
    Chunk {
        id: ChunkId(0),
        file: file.to_path_buf(),
        // tree-sitter is 0-indexed; convert to 1-indexed lines for human
        // and tooling friendliness (matches what most editors display).
        lines: (line_start + 1)..(line_end + 1),
        kind,
        name: name.to_string(),
        signature_hash: fnv1a_64(span_text.as_bytes()),
        text: span_text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extension_dispatch_picks_correct_language() {
        assert_eq!(
            SupportedLanguage::from_extension("rs"),
            Some(SupportedLanguage::Rust)
        );
        assert_eq!(
            SupportedLanguage::from_extension("py"),
            Some(SupportedLanguage::Python)
        );
        assert_eq!(
            SupportedLanguage::from_extension("pyi"),
            Some(SupportedLanguage::Python)
        );
        for ext in ["ts", "mts", "cts", "js", "mjs", "cjs"] {
            assert_eq!(
                SupportedLanguage::from_extension(ext),
                Some(SupportedLanguage::TypeScript),
                "extension {ext} should route to TypeScript",
            );
        }
        for ext in ["tsx", "jsx"] {
            assert_eq!(
                SupportedLanguage::from_extension(ext),
                Some(SupportedLanguage::Tsx),
                "extension {ext} should route to Tsx",
            );
        }
        assert_eq!(SupportedLanguage::from_extension("md"), None);
    }

    #[test]
    fn for_path_returns_none_for_unsupported_extension() {
        assert!(Chunker::for_path(&PathBuf::from("README.md")).is_none());
        assert!(Chunker::for_path(&PathBuf::from("data.json")).is_none());
    }

    #[test]
    fn for_path_works_for_supported_extension() {
        let r = Chunker::for_path(&PathBuf::from("src/lib.rs"))
            .expect("rs is supported")
            .expect("compiles");
        assert_eq!(r.language(), SupportedLanguage::Rust);
    }

    #[test]
    fn rust_extracts_struct_function_and_trait() {
        let src = r#"
pub struct FixedBuffer { data: Vec<u8> }

pub fn make_buffer() -> FixedBuffer {
    FixedBuffer { data: vec![] }
}

pub trait Readable {
    fn read(&self) -> &[u8];
}
"#;
        let chunker = Chunker::for_language(SupportedLanguage::Rust).expect("compile");
        let chunks = chunker
            .extract(src.as_bytes(), &PathBuf::from("test.rs"))
            .expect("extract");

        let names: Vec<&str> = chunks.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"FixedBuffer"),
            "expected struct in chunks, got: {names:?}"
        );
        assert!(
            names.contains(&"make_buffer"),
            "expected function in chunks, got: {names:?}"
        );
        assert!(
            names.contains(&"Readable"),
            "expected trait in chunks, got: {names:?}"
        );

        // Verify kinds are set sensibly. Upstream tags.scm uses
        // @definition.class for structs and @definition.interface for
        // traits.
        let by_name: std::collections::HashMap<_, _> =
            chunks.iter().map(|c| (c.name.as_str(), c.kind)).collect();
        assert_eq!(by_name["FixedBuffer"], ChunkKind::Class);
        assert_eq!(by_name["make_buffer"], ChunkKind::Function);
        assert_eq!(by_name["Readable"], ChunkKind::Interface);
    }

    #[test]
    fn rust_chunk_text_contains_full_definition_body() {
        let src = "pub fn answer() -> i32 { 42 }\n";
        let chunker = Chunker::for_language(SupportedLanguage::Rust).expect("compile");
        let chunks = chunker
            .extract(src.as_bytes(), &PathBuf::from("a.rs"))
            .expect("extract");
        let answer = chunks
            .iter()
            .find(|c| c.name == "answer")
            .expect("answer present");
        assert!(answer.text.contains("42"));
        assert!(answer.text.starts_with("pub fn answer"));
    }

    #[test]
    fn rust_methods_inside_impl_are_tagged_as_method_not_function() {
        // tree-sitter-rust tags.scm carries this distinction:
        //   (function_item ...) → @definition.function
        //   (declaration_list (function_item ...)) → @definition.method
        let src = r#"
struct S;
impl S {
    fn inside(&self) -> i32 { 7 }
}
fn outside() -> i32 { 8 }
"#;
        let chunker = Chunker::for_language(SupportedLanguage::Rust).expect("compile");
        let chunks = chunker
            .extract(src.as_bytes(), &PathBuf::from("a.rs"))
            .expect("extract");
        let kinds: std::collections::HashMap<_, _> =
            chunks.iter().map(|c| (c.name.as_str(), c.kind)).collect();
        assert_eq!(kinds.get("inside"), Some(&ChunkKind::Method));
        assert_eq!(kinds.get("outside"), Some(&ChunkKind::Function));
    }

    #[test]
    fn python_extracts_class_function_and_constant() {
        let src = r#"
GREETING = "hello"

def shout(msg):
    return msg.upper()

class Speaker:
    def say(self, msg):
        print(msg)
"#;
        let chunker = Chunker::for_language(SupportedLanguage::Python).expect("compile");
        let chunks = chunker
            .extract(src.as_bytes(), &PathBuf::from("speaker.py"))
            .expect("extract");

        let by_name: std::collections::HashMap<_, _> =
            chunks.iter().map(|c| (c.name.as_str(), c.kind)).collect();
        assert_eq!(by_name.get("GREETING"), Some(&ChunkKind::Constant));
        assert_eq!(by_name.get("shout"), Some(&ChunkKind::Function));
        assert_eq!(by_name.get("Speaker"), Some(&ChunkKind::Class));
    }

    #[test]
    fn signature_hash_is_stable_and_content_dependent() {
        let chunker = Chunker::for_language(SupportedLanguage::Rust).expect("compile");
        let src_a = "fn x() -> i32 { 1 }\n";
        let src_b = "fn x() -> i32 { 2 }\n";

        let a = chunker
            .extract(src_a.as_bytes(), &PathBuf::from("a.rs"))
            .unwrap();
        let b = chunker
            .extract(src_b.as_bytes(), &PathBuf::from("a.rs"))
            .unwrap();
        let a2 = chunker
            .extract(src_a.as_bytes(), &PathBuf::from("a.rs"))
            .unwrap();

        assert_eq!(a[0].signature_hash, a2[0].signature_hash, "stable");
        assert_ne!(a[0].signature_hash, b[0].signature_hash, "content-sensitive");
    }

    #[test]
    fn empty_source_yields_no_chunks() {
        let chunker = Chunker::for_language(SupportedLanguage::Rust).expect("compile");
        let chunks = chunker
            .extract(b"", &PathBuf::from("empty.rs"))
            .expect("extract");
        assert!(chunks.is_empty());
    }

    #[test]
    fn small_module_chunks_are_kept() {
        // Module declaration (no body) should survive.
        let src = "pub mod fixed_buffer;\n";
        let chunker = Chunker::for_language(SupportedLanguage::Rust).expect("compile");
        let chunks = chunker
            .extract(src.as_bytes(), &PathBuf::from("a.rs"))
            .expect("extract");
        assert!(
            chunks.iter().any(|c| c.kind == ChunkKind::Module && c.name == "fixed_buffer"),
            "small module declarations are useful navigation anchors"
        );
    }

    #[test]
    fn oversize_module_chunks_are_dropped() {
        // 50 inner functions, each with ~1KB body → mod body ~50KB,
        // exceeding MODULE_CHUNK_LIMIT_BYTES (32KB). Inner functions
        // each well under the limit; they MUST still be captured.
        let inner_body = "    let _x = 0;\n".repeat(60); // ~960 bytes
        let mut body = String::new();
        for i in 0..50 {
            body.push_str(&format!("fn filler_{i}() {{\n{inner_body}}}\n"));
        }
        let src = format!("pub mod tests {{\n{body}}}\n");
        assert!(
            src.len() > 32 * 1024,
            "test source must exceed module limit, got {}",
            src.len()
        );

        let chunker = Chunker::for_language(SupportedLanguage::Rust).expect("compile");
        let chunks = chunker
            .extract(src.as_bytes(), &PathBuf::from("a.rs"))
            .expect("extract");

        // The Module chunk for `tests` should be dropped.
        let module_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Module)
            .collect();
        assert!(
            module_chunks.is_empty(),
            "oversize module chunks must be dropped, got: {:?}",
            module_chunks.iter().map(|c| (&c.name, c.text.len())).collect::<Vec<_>>()
        );

        // Inner functions are still captured. Note: tree-sitter-rust's
        // tags.scm tags inner fns inside any `declaration_list` (mod
        // body, impl body, etc.) as `@definition.method`, NOT
        // `@definition.function` — `Function` is reserved for top-level
        // bare fns. So we accept either kind here.
        let inner_count = chunks
            .iter()
            .filter(|c| {
                c.name.starts_with("filler_")
                    && (c.kind == ChunkKind::Function || c.kind == ChunkKind::Method)
            })
            .count();
        assert_eq!(
            inner_count, 50,
            "all 50 inner fns must be captured separately, regardless of kind"
        );
    }

    #[test]
    fn oversize_function_chunks_are_kept() {
        // A 50KB function is big but legitimately one retrieval unit;
        // unlike modules, oversize functions are NOT dropped.
        let body = "    let _x = 0;\n".repeat(3500); // ~52KB body
        let src = format!("fn huge() {{\n{body}}}\n");
        let chunker = Chunker::for_language(SupportedLanguage::Rust).expect("compile");
        let chunks = chunker
            .extract(src.as_bytes(), &PathBuf::from("a.rs"))
            .expect("extract");
        let huge = chunks.iter().find(|c| c.name == "huge");
        assert!(
            huge.is_some(),
            "oversize function chunks are kept; only modules are dropped"
        );
        assert!(huge.unwrap().text.len() > 32 * 1024);
    }

    #[test]
    fn typescript_extracts_function_class_interface_and_arrow() {
        let src = r#"
function shout(msg: string): string {
    return msg.toUpperCase();
}

const greet = (name: string): string => `hi ${name}`;

class Speaker {
    say(msg: string): void {
        console.log(msg);
    }
}

interface Greetable {
    name: string;
}

type Pair<T> = [T, T];

enum Mode { On, Off }
"#;
        let chunker = Chunker::for_language(SupportedLanguage::TypeScript).expect("compile");
        let chunks = chunker
            .extract(src.as_bytes(), &PathBuf::from("speaker.ts"))
            .expect("extract");
        let by_name: std::collections::HashMap<_, _> =
            chunks.iter().map(|c| (c.name.as_str(), c.kind)).collect();

        assert_eq!(by_name.get("shout"), Some(&ChunkKind::Function));
        assert_eq!(by_name.get("greet"), Some(&ChunkKind::Function));
        assert_eq!(by_name.get("Speaker"), Some(&ChunkKind::Class));
        // Methods are inside class bodies; precedence rule prefers method
        // over function (mirrors the Rust impl-method case).
        assert_eq!(by_name.get("say"), Some(&ChunkKind::Method));
        assert_eq!(by_name.get("Greetable"), Some(&ChunkKind::Interface));
        assert_eq!(by_name.get("Pair"), Some(&ChunkKind::Type));
        assert_eq!(by_name.get("Mode"), Some(&ChunkKind::Enum));
    }

    #[test]
    fn tsx_parses_jsx_in_arrow_components() {
        // The TypeScript grammar would reject the `<div>...</div>` in the
        // body; TSX must accept it. Verifies the Tsx variant routes to a
        // different parser even though it shares tags.scm with TypeScript.
        let src = r#"
const Hello = (props: { name: string }) => <div>hi {props.name}</div>;
"#;
        let chunker = Chunker::for_language(SupportedLanguage::Tsx).expect("compile");
        let chunks = chunker
            .extract(src.as_bytes(), &PathBuf::from("Hello.tsx"))
            .expect("extract");
        assert!(
            chunks.iter().any(|c| c.name == "Hello" && c.kind == ChunkKind::Function),
            "Hello arrow component should be captured as a function chunk"
        );
    }

    #[test]
    fn javascript_files_route_through_typescript_grammar() {
        // .js files map to SupportedLanguage::TypeScript — TS is a syntactic
        // superset of JS (modulo JSX), so plain JS parses correctly.
        let chunker = Chunker::for_path(&PathBuf::from("util.js"))
            .expect("js is supported")
            .expect("compile");
        assert_eq!(chunker.language(), SupportedLanguage::TypeScript);
        let src = "function plain() { return 1; }\n";
        let chunks = chunker
            .extract(src.as_bytes(), &PathBuf::from("util.js"))
            .expect("extract");
        assert!(chunks.iter().any(|c| c.name == "plain"));
    }

    #[test]
    fn lines_are_one_indexed() {
        let src = "\n\nfn foo() {}\n";
        let chunker = Chunker::for_language(SupportedLanguage::Rust).expect("compile");
        let chunks = chunker
            .extract(src.as_bytes(), &PathBuf::from("a.rs"))
            .expect("extract");
        let foo = chunks
            .iter()
            .find(|c| c.name == "foo")
            .expect("foo present");
        // foo starts on the third line (1-indexed: 3); ends on same line.
        assert_eq!(foo.lines, 3..3);
    }
}
