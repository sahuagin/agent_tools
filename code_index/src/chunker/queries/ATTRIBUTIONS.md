# Vendored tags.scm queries

The `tags.scm` files under this directory are vendored from the upstream
tree-sitter grammar repositories. Each is governed by its own license (MIT
in all current cases) which travels with the file.

## Sources (pinned)

- `rust/tags.scm` from
  https://github.com/tree-sitter/tree-sitter-rust/blob/77a3747266f4d621d0757825e6b11edcbf991ca5/queries/tags.scm
  (MIT, Copyright (c) 2017 Maxim Sokolov)
- `python/tags.scm` from
  https://github.com/tree-sitter/tree-sitter-python/blob/26855eabccb19c6abf499fbc5b8dc7cc9ab8bc64/queries/tags.scm
  (MIT, MIT-licensed via the upstream repo's LICENSE)

- `typescript/tags.scm` is hand-authored, not vendored. The
  tree-sitter-typescript repo (https://github.com/tree-sitter/tree-sitter-typescript)
  does not ship a tags.scm — it provides only highlights/locals/indents
  queries. Our patterns are derived from two reference sources:
  - tree-sitter-typescript's own grammar conventions (the node names
    `function_declaration`, `class_declaration`, `method_definition`,
    `interface_declaration`, `type_alias_declaration`, etc. are the
    grammar's published surface).
  - aider's typescript-tags.scm
    (https://github.com/Aider-AI/aider/blob/main/aider/queries/tree-sitter-languages/typescript-tags.scm,
    Apache-2.0) for cross-checking which patterns are typically used to
    drive ctags-style symbol indexing.
  The same file is used for both the TypeScript and TSX grammars (their
  declaration node-types are identical; only JSX differs).
  Hand-authored under BSD-3-Clause along with the rest of code_index.

## Updating

Re-fetch from the same path on a newer commit and bump the SHA recorded
above. `code_index` itself is BSD-3-Clause (see `LICENSE`); the vendored
queries remain MIT and are dual-distributed under their original terms.
