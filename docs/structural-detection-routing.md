# What syntax can decide, and what it cannot

Bead at-ast-query-detection-t4u.2. Companion to
`structural-detection-prior-art.md`. Every row below was run against `mu` at
2026-09-24 main with ast-grep 0.45.3, except where marked as reasoning about a
bucket we have no engine for on this platform.

A shape is only worth writing once we know which bucket decides it. Attempting
a type question with a syntax engine produces a rule that looks like it works
and silently under-reports.

## The three buckets

**Syntax** — tree-sitter / ast-grep can decide it. Node shapes, literals,
containment within the same file.

**Types** — needs name resolution or type information: `dylint` (a lint crate
against rustc's HIR) or a language server. Clippy's `await_holding_lock` is the
canonical prior art for a shape in this bucket.

**Dataflow** — needs taint tracking across functions and files: CodeQL or
Joern, neither of which runs on FreeBSD. For us this bucket means "not
mechanically checkable today", and a shape landing here should be stated as
such rather than approximated.

## Measured rows

| shape | bucket | result on mu | notes |
| --- | --- | --- | --- |
| a call to a named function (`unwrap()`, `fs::write`, `File::create`) | syntax | 1225 / 163 sites, ~0.2 s | clean |
| the same call restricted to a test module | syntax | 897 of 1225 unwraps inside `mod tests` | relational `inside` + `has field: name` |
| a const holding an endpoint-shaped literal | syntax | 12 sites | `kind: const_item` + `has: string_literal` with a regex; these particular twelve are legitimate defaults in mu, which is what a baseline is for |
| a sleep inside a loop (the poll-loop shape) | syntax | 33 sites | fires in both real poll loops and legitimate backoff; HINT grade at best without more constraint |
| a filesystem write inside module *X* | **syntax, but not the way it looks** | 0 sites with `inside: mod_item` | see below |
| a lock guard held across an `.await` | types | not attempted | approximable syntactically (a `lock()` binding in a block that later contains `.await`) but deciding the binding's type needs resolution; clippy already implements it properly |
| "is this `open` ours or std's" | types | — | same reason |
| "does a transcript reach a cloud provider" | dataflow | — | no engine on this platform |

## The module-scoping trap

"Inside module X" is the natural way to state most of our anti-patterns — a
filesystem write inside the modules that should be going through the ring
buffer, a direct provider call inside the code that should route through the
dispatcher. Expressed as `inside: { kind: mod_item }` it matches **nothing**,
and the rule looks correct while finding zero sites.

The reason is that Rust modules are usually *files*, not `mod { }` blocks.
`crates/mu-irc-gateway/src/transport.rs` is the `transport` module and contains
no `mod_item` node at all. AST containment can only see what is lexically
inside the parsed file.

So module scope belongs to the path glob, which `invariant-audit` already has,
and AST containment handles the finer scope inside a file:

```toml
paths = ["crates/mu-irc-gateway/src/{transport,bridge/*}.rs"]   # the module
# rule: fs::write(...)  inside: { kind: function_item, has: { field: name, regex: "^run$" } }
```

That split is a design input for the shape in at-ast-query-detection-t4u.4:
the structural shape must compose with `paths`, not replace it. A rule that
tries to express module membership structurally will be quietly empty.

## Consequences for the corpus

- Every shape in at-ast-query-detection-t4u.3 carries its bucket, and shapes in
  the types or dataflow buckets are recorded with the engine that could decide
  them rather than attempted syntactically.
- A syntax-bucket shape whose scope is a module is written as `paths` plus a
  rule, and its test must include a site in a *different* module that must not
  fire — otherwise the path glob is doing all the work and nobody notices.
- Any rule returning zero sites on first run is suspect until a deliberately
  planted positive proves it can fire. Zero is what a wrong rule looks like.
