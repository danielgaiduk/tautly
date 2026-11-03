; Definitions only. Methods are told apart from functions in code (a def directly
; inside a class body); module-level assignments count when the name is UPPER_CASE.
(function_definition name: (identifier) @name) @definition.function
(class_definition name: (identifier) @name) @definition.type
(module
  (expression_statement
    (assignment left: (identifier) @name)) @definition.constant)
