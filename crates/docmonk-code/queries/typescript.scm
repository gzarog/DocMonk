; TypeScript query file -- see python.scm for the shared capture-name
; contract. Also used for .tsx (parser.py maps that extension to the
; "tsx" grammar but reuses this file; JSX-only nodes are irrelevant here).

(class_declaration name: (type_identifier) @class.name) @class.definition
(interface_declaration name: (type_identifier) @interface.name) @interface.definition
(enum_declaration name: (identifier) @enum.name) @enum.definition

(extends_clause value: (_) @extends.object)
(implements_clause (type_identifier) @implements.object)

(function_declaration name: (identifier) @function.name) @function.definition
(method_definition name: (property_identifier) @function.name) @function.definition
(method_signature name: (property_identifier) @function.name) @function.definition

(public_field_definition name: (property_identifier) @field.name) @field.definition

(import_statement source: (string (string_fragment) @import.module)) @import.statement

(call_expression function: [(identifier) (member_expression)] @call.callee) @call.expression
(new_expression constructor: (identifier) @call.callee) @call.expression

; Decorators are siblings of the declaration they annotate (not nested
; inside it), so extractor.py's structural parent walk cannot associate
; them; ``.`` anchors the match to the immediately-following declaration
; instead, and extractor.py correlates by that shared node identity.
(class_body (decorator) @decorator.raw . (method_definition) @decorator.subject)
(class_body (decorator) @decorator.raw . (public_field_definition) @decorator.subject)
