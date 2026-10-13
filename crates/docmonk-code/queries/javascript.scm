; JavaScript query file -- see python.scm for the shared capture-name
; contract. JS has no interfaces/enums/structs, so only class/function/
; field entities are emitted.

(class_declaration name: (identifier) @class.name) @class.definition
(class_heritage (identifier) @extends.object)

(function_declaration name: (identifier) @function.name) @function.definition
(method_definition name: (property_identifier) @function.name) @function.definition

(field_definition property: (property_identifier) @field.name) @field.definition

(import_statement source: (string (string_fragment) @import.module)) @import.statement

(call_expression function: [(identifier) (member_expression)] @call.callee) @call.expression
(new_expression constructor: (identifier) @call.callee) @call.expression
