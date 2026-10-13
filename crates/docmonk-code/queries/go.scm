; Go query file -- see python.scm for the shared capture-name contract.
;
; Go methods are declared outside the struct/interface body and linked
; only by a receiver type name, so they cannot be found via structural
; nesting: ``function.parent_name`` gives extractor.py an explicit,
; name-based parent instead (looked up among this file's/project's
; already-collected type entities, the same mechanism Rust's ``impl``
; blocks use).

(package_clause (package_identifier) @namespace.name) @namespace.definition

(type_spec
  name: (type_identifier) @struct.name
  type: (struct_type)) @struct.definition

(type_spec
  name: (type_identifier) @interface.name
  type: (interface_type)) @interface.definition

(field_declaration name: (field_identifier) @field.name) @field.definition

(interface_type
  (method_elem name: (field_identifier) @function.name) @function.definition)

(function_declaration name: (identifier) @function.name) @function.definition

(method_declaration
  receiver: (parameter_list
    (parameter_declaration
      type: [
        (pointer_type (type_identifier) @function.parent_name)
        (type_identifier) @function.parent_name
      ]))
  name: (field_identifier) @function.name) @function.definition

(import_spec path: (interpreted_string_literal (interpreted_string_literal_content) @import.module)) @import.statement

(call_expression function: [(identifier) (selector_expression)] @call.callee) @call.expression
