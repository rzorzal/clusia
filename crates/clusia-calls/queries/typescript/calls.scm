(call_expression function: (identifier) @name) @call
(call_expression function: (member_expression object: (_) @receiver property: (property_identifier) @name)) @call
(new_expression constructor: (identifier) @constructor) @call
