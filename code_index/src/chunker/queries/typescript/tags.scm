; TypeScript / TSX tag captures.
;
; Definition tags map to ChunkKinds via chunk_kind_from_tag.
; Reference tags map to EdgeKinds via edge_kind_from_reference_tag.
;
; The tree-sitter-typescript crate exposes two grammars (TypeScript and
; TSX). Their node-type schemas for declarations and expressions are
; identical, so this single tags.scm is shared by both. JSX-specific
; nodes (jsx_element, jsx_self_closing_element) are intentionally NOT
; tagged — they aren't definitions in the chunking sense.

; -- Definitions --

; Function declarations: `function foo() {}`
(function_declaration
  name: (identifier) @name) @definition.function

; Generator function declarations: `function* foo() {}`
(generator_function_declaration
  name: (identifier) @name) @definition.function

; Function signatures (e.g. in .d.ts files or interfaces):
; `declare function foo(): void;`
(function_signature
  name: (identifier) @name) @definition.function

; Arrow functions / function expressions assigned to a const/let/var:
; `const foo = () => {}` and `const foo = function() {}`
;
; Common in modern TS — without this we miss most of a typical codebase.
; The chunk span is the variable_declarator (matches helix's convention).
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)]) @definition.function)

(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)]) @definition.function)

; Class declarations
(class_declaration
  name: (type_identifier) @name) @definition.class

; Abstract class declarations
(abstract_class_declaration
  name: (type_identifier) @name) @definition.class

; Method definitions (regular class methods)
(method_definition
  name: (property_identifier) @name) @definition.method

; Method signatures (in interfaces or type literals)
(method_signature
  name: (property_identifier) @name) @definition.method

; Abstract method signatures (in abstract classes)
(abstract_method_signature
  name: (property_identifier) @name) @definition.method

; Interface declarations
(interface_declaration
  name: (type_identifier) @name) @definition.interface

; Type alias declarations
(type_alias_declaration
  name: (type_identifier) @name) @definition.type

; Enum declarations
(enum_declaration
  name: (identifier) @name) @definition.enum

; Module / namespace declarations
(module
  name: (identifier) @name) @definition.module

; -- References --

; Direct function calls: `foo(...)`
(call_expression
  function: (identifier) @name) @reference.call

; Method/property calls: `obj.foo(...)`
(call_expression
  function: (member_expression
    property: (property_identifier) @name)) @reference.call

; Constructor calls: `new Foo(...)`. The TypeScript grammar uses
; `(identifier)` for the constructor field — `(type_identifier)` is not
; valid in this position (verified by query compile failure).
(new_expression
  constructor: (identifier) @name) @reference.class

; Type references in annotations: `let x: Foo`, `function f(): Foo`
(type_annotation
  (type_identifier) @name) @reference.type
