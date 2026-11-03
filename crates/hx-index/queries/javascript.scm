; Definitions only.
(function_declaration name: (identifier) @name) @definition.function
(generator_function_declaration name: (identifier) @name) @definition.function
(class_declaration name: (identifier) @name) @definition.type
(method_definition name: (property_identifier) @name) @definition.method
; Top-level `const`/`let`: code turns function-valued ones into functions and keeps
; only `const` for the rest.
(program
  (lexical_declaration
    (variable_declarator name: (identifier) @name) @declarator) @definition.constant)
(program
  (export_statement
    declaration: (lexical_declaration
      (variable_declarator name: (identifier) @name) @declarator) @definition.constant))
