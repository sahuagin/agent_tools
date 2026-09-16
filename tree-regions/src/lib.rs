//! Regions of a source file, read off the tree-sitter parse tree instead of a
//! hand-written lexer: today, the `#[cfg(test)]`-attributed items of a Rust
//! file (`cfg_test_regions`) and the file's parse errors (`parse_errors`).
//! Shared by `invariant-audit` (which drops violation sites inside test
//! regions) and `code_index` (which down-weights test chunks in recall).
//! agent_tools bead at-zzb.

use std::ops::Range;

use tree_sitter::{Node, Parser, Tree};

/// A byte range of the source with the 1-based line range it spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub bytes: Range<usize>,
    /// 1-based, inclusive.
    pub lines: Range<usize>,
}

impl Region {
    fn of(node: Node<'_>) -> Self {
        Self {
            bytes: node.byte_range(),
            lines: node.start_position().row + 1..node.end_position().row + 1,
        }
    }

    /// Does the byte offset lie inside this region?
    pub fn contains(&self, byte: usize) -> bool {
        self.bytes.contains(&byte)
    }
}

/// A parse error the grammar reported: the audit refuses to reason about a
/// file it could not parse rather than guess where regions end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line and 0-based column of the error node.
    pub line: usize,
    pub column: usize,
    /// `true` for a token the grammar had to skip (an `ERROR` node), `false`
    /// for one it had to invent (a `MISSING` node).
    pub unexpected: bool,
}

/// Parse a Rust source. `None` only if tree-sitter itself failed (a
/// cancelled or timed-out parse); syntax errors still yield a tree, see
/// [`parse_errors`].
pub fn parse_rust(source: &[u8]) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("the compiled-in Rust grammar matches the tree-sitter runtime");
    parser.parse(source, None)
}

/// Every `ERROR` or `MISSING` node in the tree, in document order.
pub fn parse_errors(tree: &Tree) -> Vec<ParseError> {
    let mut out = Vec::new();
    if !tree.root_node().has_error() {
        return out;
    }
    let mut cursor = tree.root_node().walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            let p = node.start_position();
            out.push(ParseError {
                line: p.row + 1,
                column: p.column,
                unexpected: node.is_error(),
            });
            continue;
        }
        if node.has_error() {
            let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }
    out
}

/// The regions of a Rust file that are `#[cfg(test)]`-attributed items:
/// each is the attribute together with the item it decorates (a module, a
/// function, a `use`, a struct field, an enum variant, a statement — whatever
/// the grammar attaches the attribute to), as the parse tree delimits it.
/// Nested test modules fall inside their parent's region. Only the exact
/// form `#[cfg(test)]` counts; `cfg(all(test, ...))` and other predicates are
/// not test regions here.
///
/// The attribute is found where the grammar found it, so one inside a
/// comment, a string, or a doc comment is never an attribute; and an item's
/// end is the grammar's, so a `{` in a string, a `<` as a comparison, or a
/// `,` in a generic list cannot move it.
pub fn cfg_test_regions(source: &[u8], tree: &Tree) -> Vec<Region> {
    let mut out = Vec::new();
    let mut cursor = tree.root_node().walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            if child.kind() == "attribute_item" && is_cfg_test(child, source) {
                // The decorated item is the next non-attribute sibling: outer
                // attributes stack, and the grammar keeps them as siblings
                // preceding the item.
                let mut j = i + 1;
                while j < children.len()
                    && (children[j].kind() == "attribute_item"
                        || children[j].kind() == "line_comment"
                        || children[j].kind() == "block_comment")
                {
                    j += 1;
                }
                if j < children.len() {
                    let item = children[j];
                    out.push(Region {
                        bytes: child.start_byte()..item.end_byte(),
                        lines: child.start_position().row + 1..item.end_position().row + 1,
                    });
                    // nothing inside the item needs a separate region
                    i = j + 1;
                    continue;
                }
                // an attribute with nothing after it (end of a block): just
                // its own extent
                out.push(Region::of(child));
                i += 1;
                continue;
            }
            stack.push(child);
            i += 1;
        }
    }
    out.sort_by_key(|r| r.bytes.start);
    out
}

/// Is this `attribute_item` exactly `#[cfg(test)]` (whitespace permitted)?
fn is_cfg_test(attr: Node<'_>, source: &[u8]) -> bool {
    // attribute_item > attribute > (identifier "cfg") (token_tree "(test)")
    let attribute = match attr.child_by_field_name("attribute") {
        Some(a) => a,
        None => {
            let mut c = attr.walk();
            let found = attr.children(&mut c).find(|n| n.kind() == "attribute");
            match found {
                Some(a) => a,
                None => return false,
            }
        }
    };
    let mut c = attribute.walk();
    let parts: Vec<Node<'_>> = attribute.children(&mut c).collect();
    let text = |n: Node<'_>| std::str::from_utf8(&source[n.byte_range()]).unwrap_or("");
    let is_cfg = parts
        .first()
        .map(|n| n.kind() == "identifier" && text(*n) == "cfg")
        .unwrap_or(false);
    if !is_cfg {
        return false;
    }
    let Some(args) = parts.iter().find(|n| n.kind() == "token_tree") else {
        return false;
    };
    let inner: String = text(*args).chars().filter(|c| !c.is_whitespace()).collect();
    inner == "(test)"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regions(src: &str) -> Vec<Range<usize>> {
        let tree = parse_rust(src.as_bytes()).unwrap();
        assert!(parse_errors(&tree).is_empty(), "fixture must parse: {src}");
        cfg_test_regions(src.as_bytes(), &tree)
            .into_iter()
            .map(|r| r.lines)
            .collect()
    }

    #[test]
    fn inline_module_with_nested_module_is_one_region() {
        let src = "fn p() {}\n#[cfg(test)]\nmod tests {\n    mod inner { fn t() {} }\n    fn u() {}\n}\nfn q() {}\n";
        assert_eq!(regions(src), vec![2..6]);
    }

    #[test]
    fn attributes_stack_and_comments_between_do_not_break_the_pair() {
        let src = "#[cfg(test)]\n#[allow(dead_code)]\n// why\nmod tests {}\nfn p() {}\n";
        assert_eq!(regions(src), vec![1..4]);
    }

    #[test]
    fn same_line_item_and_the_next_item_are_separate() {
        let src = "#[cfg(test)] fn h() {} fn p() {}\n";
        let tree = parse_rust(src.as_bytes()).unwrap();
        let r = cfg_test_regions(src.as_bytes(), &tree);
        assert_eq!(r.len(), 1);
        let p_at = src.find("fn p").unwrap();
        assert!(!r[0].contains(p_at), "{r:?}");
        assert!(r[0].contains(src.find("fn h").unwrap()));
    }

    #[test]
    fn attribute_in_comment_or_string_is_not_an_attribute() {
        let src = "/*\n#[cfg(test)]\n*/\nfn p() {}\nconst S: &str = r#\"\n#[cfg(test)]\n}\"#;\nfn q() {}\n";
        assert_eq!(regions(src), Vec::<Range<usize>>::new());
    }

    #[test]
    fn braceless_items_fields_variants_and_statements() {
        let src = "#[cfg(test)]\nuse std::thread::sleep;\nstruct S {\n    #[cfg(test)]\n    f: Vec<(u8, u8)>,\n    g: u8,\n}\nenum E {\n    #[cfg(test)]\n    Only\n}\nfn f() {\n    #[cfg(test)]\n    let x = 1 < 2;\n    let y = 2;\n}\n#[cfg(test)] mod out;\nfn p() {}\n";
        assert_eq!(regions(src), vec![1..2, 4..5, 9..10, 13..14, 17..17]);
    }

    #[test]
    fn operators_generics_and_where_clauses_do_not_move_the_end() {
        let src = "#[cfg(test)] const F: bool = 1 < 2;\n#[cfg(test)] const M: u32 = 1 << 4;\n#[cfg(test)] const V: Option<Vec<(u8, u8)>> = None;\n#[cfg(test)]\nfn w<T>(t: T) where T: Ord, { let _ = t; }\nfn p() {}\n";
        assert_eq!(regions(src), vec![1..1, 2..2, 3..3, 4..5]);
    }

    #[test]
    fn only_the_exact_predicate_counts() {
        let src = "#[cfg(all(test, unix))]\nfn a() {}\n#[cfg(not(test))]\nfn b() {}\n#[cfg( test )]\nfn c() {}\n#[cfg_attr(test, allow(dead_code))]\nfn d() {}\n";
        assert_eq!(regions(src), vec![5..6]);
    }

    #[test]
    fn literals_cannot_end_an_item_early() {
        let src = "#[cfg(test)]\nmod t {\n    const A: &str = \"}\";\n    const B: &[u8] = b\"}\";\n    const C: char = '}';\n    const D: &str = r\"multi\n}\nline\";\n    const E: &CStr = c\"}\";\n    fn t() {}\n}\nfn p() {}\n";
        assert_eq!(regions(src), vec![1..11]);
    }

    #[test]
    fn parse_errors_are_reported_not_guessed() {
        let src = "#[cfg(test)]\nmod t {\n    fn t() {}\n// never closed\n";
        let tree = parse_rust(src.as_bytes()).unwrap();
        let errs = parse_errors(&tree);
        assert!(!errs.is_empty());
    }
}
