(function_declaration name: (identifier) @name) @def
(variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @def
(class_declaration name: (_) @qualifier body: (class_body (method_definition name: (_) @name) @def))
