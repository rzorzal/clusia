(function_definition name: (identifier) @name) @def
(class_definition name: (identifier) @qualifier body: (block (function_definition name: (identifier) @name) @def))
(class_definition name: (identifier) @qualifier body: (block (decorated_definition definition: (function_definition name: (identifier) @name) @def)))
