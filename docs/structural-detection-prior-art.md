# Structural detection: prior art, and what runs here

Bead at-ast-query-detection-t4u.1. Measured 2026-09-24 on aiteam
(FreeBSD 15.1-RELEASE-p1 amd64) against the `mu` repo at that day's main
(289 files, ~7.5k chunks). Every number below came from a run, not from
documentation.

The question is not "what is tree-sitter". It is **which engine, if any, do we
adopt, and what is left for us to build** — including the answer "we write no
query engine at all".

## What actually runs here

Prebuilt binaries are the wrong question on FreeBSD — nobody ships them and
open source gets built. The real question per engine is which toolchain the
build needs and whether that toolchain is available.

| engine | language | build path here | verdict |
| --- | --- | --- | --- |
| **ast-grep** | Rust | `cargo install ast-grep --locked` → 0.45.3, **built and verified** | usable today |
| **Semgrep** | Python wrapper over an OCaml core | no port; `ocaml-4.14.2` and `ocaml-opam-2.5.2` ARE in ports, so semgrep-core is an opam source build, with `wheelfs` available to present the Python half | buildable, not attempted |
| **Comby** | OCaml | no port; same opam route | buildable, not attempted |
| **CodeQL CLI** | prebuilt, closed | no source build | not available |
| **Joern** | JVM | would need a JDK; the operator does not want Java installed | ruled out by choice |
| **dylint** | Rust | builds from source | usable; type-aware, see below |
| **raw tree-sitter** | Rust, already vendored | `tree-regions` already depends on tree-sitter 0.25 + tree-sitter-rust 0.24 | usable; the build-it-ourselves baseline |

"Not attempted" is the honest status for Semgrep and Comby: an opam build of
someone else's OCaml project is a day of work with an uncertain end, and
ast-grep already satisfies the requirement at 0.2 s. If ast-grep's rule
language ever proves too weak, Semgrep's registry of existing rules is the
reason to spend that day, and this row is where to start rather than
re-deriving that it "has no FreeBSD wheel" — there are never FreeBSD wheels.

The review gate runs on this jail, so whatever is chosen has to build here.

## What ast-grep actually does on our code

Whole `crates/` tree of mu, warm:

```
$X.unwrap()                          1225 matches   0.193s
fs::write / File::create (4 forms)    163 matches   0.202s
unwrap() inside `mod tests`            897 matches   ~0.2s
```

Two hundred milliseconds over the workspace answers the "should be quite fast"
question: yes, and by a wide margin. No model, no network, no build of the
target repo.

**Relational rules are the reason to prefer it over raw tree-sitter.** The
shapes we care about are scope-sensitive — "a filesystem write *inside* the
modules that are supposed to use the ring buffer", not "a filesystem write". In
ast-grep that is a YAML rule with `inside:` / `has:` / `follows:` /
`precedes:` and a `stopBy`. The tree-sitter query language has no ancestor
operator, so the in-house route means implementing ancestor traversal and a
rule format ourselves, then maintaining both.

The third row above is the interesting one: separating test code from non-test
code structurally (897 of 1225 unwraps are inside `mod tests`) is something
`invariant-audit` currently achieves with a bespoke `skip_cfg_test` pass over
`tree-regions`. A relational rule expresses it directly.

## The ceiling: macro bodies are invisible

Tree-sitter parses a Rust macro invocation's body as an opaque token tree, so
**no tree-sitter-based engine can see inside `assert!`, `assert_eq!`,
`println!`, `format!`, `write!`, `vec!`, `json!`, `matches!` or any other
macro**. Controlled probe — six functions, each containing one `.unwrap()` or
`fs::write`:

```rust
fn a() { let v = thing().unwrap(); }                          // MATCHED
fn b() { assert!(thing().unwrap().is_none()); }               // missed
fn c() { println!("{}", thing().unwrap()); }                  // missed
fn d() { let _ = vec![thing().unwrap()]; }                    // missed
fn e() { std::fs::write("p", b"x").unwrap(); }                // MATCHED
fn f() { assert_eq!(std::fs::write("p", b"x").is_ok(), true); }  // missed
```

On mu, grep finds `.unwrap()` on 1514 lines and ast-grep reports 1197 distinct
start lines. Classifying the difference: **155 are genuinely inside macro
bodies**, 1 is a comment, and ~429 are line-attribution artifacts (ast-grep
reports the start line of a multi-line expression, grep reports the line the
text is on) rather than misses. So for this pattern in this repo the true blind
spot is on the order of 10% of occurrences, concentrated in test assertions.

The consequence for enforcement is false NEGATIVES, and it is structural: a
violation written inside a macro cannot be detected this way, by any of these
tools, ever. Regex `content` matching has the opposite profile — it sees macro
bodies fine and cannot tell code from a comment or a string.

**So the existing `content` shape is not superseded by a structural shape; the
two are complementary.** Recall belongs to text matching, precision to AST
matching. Any shape promoted to enforcement should be stated with which of the
two it uses and what it therefore cannot see.

## What needs a different engine entirely

ast-grep matches syntax. It does not resolve types, follow dataflow, or cross
files. Shapes that need resolution — "is this `open` ours or std's", "is this
binding a `MutexGuard` held across an `.await`" — need `dylint` (a lint crate
compiled against rustc's HIR; clippy's `await_holding_lock` is the prior art for
exactly that shape) or a language server. Shapes that need taint — "does a
transcript ever reach a cloud provider" — need CodeQL or Joern, neither of which
runs here. Routing per shape is bead at-ast-query-detection-t4u.2.

## On the macro ceiling, in proportion

The blind spot is real but it is not a reason to withhold the capability. We
have none of this utility today; a shape that catches most of its instances is
a large improvement over a prose rule that catches none. The ceiling matters
for how a shape is GRADED — enforcement may not claim completeness — not for
whether it is worth having. The case to watch is a codebase where macros carry
real logic rather than assertions and formatting: operator and comparison impls
generated to remove boilerplate, for instance, are invisible in exactly the way
that matters.

## Recommendation

**Wrap ast-grep; do not build a query engine.**

- `invariant-audit` gains a shape that delegates to ast-grep and parses its
  `--json` output into the existing `Site` type, so ratchet, baselines pinned to
  BASE, `--report`, and the settings excludes all keep working unchanged.
- The rule text lives in the repo next to `.invariants.toml`, which keeps rules
  reviewable in the PR that introduces them.
- Pin the exact version (0.45.3 today) and record that it is a from-source
  build on this platform, because there is no binary to fall back to.
- Keep the `content` shape. It is the recall half.
- `dylint` is the escape hatch for the type-aware shapes, evaluated only if a
  wanted shape lands in that bucket.

What is given up by not building in-house: one fewer external dependency, and
rules expressed in tree-sitter's own query syntax rather than ast-grep's YAML.
Against that, building in-house means writing ancestor traversal, a rule
format, and a matcher, to arrive at something ast-grep already does in 0.2s —
and the macro ceiling applies identically to both, so nothing is bought back
where it actually hurts.
