(module (expression_statement (assignment left: (identifier) @name) @definition.constant))

(class_definition
  name: (identifier) @name) @definition.class

(function_definition
  name: (identifier) @name) @definition.function

(call
  function: (identifier) @name) @reference.call

(call
  function: (attribute
    object: (_) @receiver
    attribute: (identifier) @name)) @reference.call
