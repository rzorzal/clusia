(call_expression function: (identifier) @name) @call
(call_expression function: (scoped_identifier path: (_) @qualifier name: (identifier) @name)) @call
(call_expression function: (field_expression value: (_) @receiver field: (field_identifier) @name)) @call
(call_expression function: (generic_function function: (identifier) @name)) @call
