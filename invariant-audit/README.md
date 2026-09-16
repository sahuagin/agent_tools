# invariant-audit

Enumerate every site of an architecture-invariant violation shape and ratchet
the count. A repository describes each invariant in `.invariants.toml` as a
shape (a regex over a path scope, a bare path glob, or a Cargo dependency) plus
`baseline`, the number of sites it has today; the audit lists every site and
fails when a count rises. Baselines only go down, and are pinned to a BASE
revision so a change cannot raise one in the commit that adds the violation.

This is the installed form of mu's `scripts/invariant-audit.py` (mu #628),
with `#[cfg(test)]` regions read off the tree-sitter parse tree rather than a
lexer. Consumers install it; nobody copies it (agent_tools bead at-zzb).

## Install

```sh
cargo install --git https://github.com/sahuagin/agent_tools invariant-audit
# or, from a checkout: cargo build --release -p invariant-audit and symlink
# ~/.local/bin/invariant-audit -> target/release/invariant-audit
```

Pin the version in the consuming repo's toolchain notes the way you pin
`cargo`, `jj` or `ruff`.

## Run

```
invariant-audit                     audit; exit 1 on any count above baseline
invariant-audit --report            list every site, never fail
invariant-audit --base REV          revision whose shapes file pins the
                                    baselines (default $MU_INVARIANTS_BASE or main)
invariant-audit --base-config PATH  that file, already extracted (CI)
invariant-audit --no-base           the checkout's own baselines (first commit,
                                    push to main, throwaway repos)
invariant-audit --root PATH         repository root (default: the enclosing
                                    jj/git root of the current directory)
invariant-audit --config PATH       shapes file (default <root>/.invariants.toml)
invariant-audit --no-cache          do not read or write the region cache
```

Exit codes: `0` clean, `1` ratchet failure, `2` the audit could not run (a bad
shapes file, a bad regex, an unreadable or unparseable scoped file, BASE
requested but unresolvable).

## Shapes file

```toml
[settings]
exclude = [".git/**", ".jj/**", "target/**"]   # default excludes, overridable

[[invariant]]
id = "no-new-sleep-or-poll-sites"
rule = "the sentence from AGENTS.md; printed with every violation"
kind = "content"                  # "path", or "cargo-dependency"
paths = ["crates/**/*.rs"]        # root-relative globs: `x.md` is the root file,
                                  # `**/x.md` any x.md
exclude = ["**/tests/**"]         # optional, per invariant
pattern = 'thread::sleep\('       # content kind only
ignore_case = false               # content kind only
skip_cfg_test = true              # content kind, Rust files: matches inside a
                                  # `#[cfg(test)]`-attributed item do not count
crate = "mu-core"                 # cargo-dependency kind only
baseline = 41                     # sites the repo has today
retired = false                   # keep, do not delete, an entry BASE still has
```

### Ratchet rules

- The effective ceiling is the lower of BASE's and the checkout's baseline; an
  invariant absent from BASE is new and starts at the checkout's value.
- The shape is pinned as well as the number: when a checkout changes an
  invariant's kind, paths, exclude, pattern, crate or `skip_cfg_test`, BASE's
  shape is still counted against this checkout and held to BASE's ceiling, so
  narrowing a shape cannot hide a new site; the new shape records its own exact
  count. (Turning on `skip_cfg_test` is such a change: it lowers the count in
  one PR without the ratchet objecting.)
- Fewer sites than the checkout's baseline also fails in gate mode; lower the
  number in the same change, so the recorded count is always the true one.
- An invariant present at BASE may not silently disappear: keep the entry
  with `retired = true`. A retired entry is not counted.
- `[settings].exclude` is environmental (a venv, a symlink the walk refuses):
  the BASE pass walks with the union of BASE's and the checkout's, so an
  exclusion this checkout needs can pass its own gate. Shape-level `exclude`
  stays pinned to BASE.

### `skip_cfg_test`

A `#[cfg(test)]`-attributed item is whatever the Rust grammar attaches the
attribute to: an inline `mod tests { … }`, a function, a `use`, a struct field,
an enum variant, a statement. Its extent is the parse tree's, so a brace in a
string, a `<` used as a comparison, or a comma in a generic list cannot move
it, and an attribute inside a comment or a string is not an attribute. Nested
test modules are inside their parent's. Only the exact predicate `cfg(test)`
counts; `cfg(all(test, …))` and friends are left to `exclude`. A line is a site
if any match on it lies outside every test region. The skipped count is
printed next to the counted one so the number stays honest:

```
no-new-sleep-or-poll-sites: 41 site(s) (22 in #[cfg(test)] skipped), baseline 41 — ok
```

A Rust file the grammar cannot parse fails the audit (exit 2) naming the first
bad token: the ratchet does not guess where test code ends. Fix the file or
exclude it.

Regions are cached per file by content hash under `target/invariant-audit/`
(or `.invariant-audit-cache/` when there is no `target/`), so unchanged files
are never reparsed: on mu's 252 Rust files a cfg(test)-aware shape runs in
about 0.12 s cold and 0.02 s warm.

## The shared library

`tree-regions` (this workspace) holds the parse-tree region logic:
`parse_rust`, `parse_errors`, `cfg_test_regions`. `code_index` uses the same
function to classify a definition inside a `#[cfg(test)]` item as a test chunk,
so recall down-weights inline test modules without name or path heuristics.
