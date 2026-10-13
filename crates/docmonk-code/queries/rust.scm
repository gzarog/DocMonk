; Rust query file -- see python.scm for the shared capture-name contract.
;
; ``impl Type { ... }`` and ``impl Trait for Type { ... }`` blocks are not
; entities in their own right (the type already exists as a struct/enum);
; extractor.py's generic ``function.parent_name`` mechanism (also used by
; Go's receiver methods) attaches their functions to the existing type by
; name instead of creating a duplicate entity for the impl block.
; ``implements.subject``/``implements.object`` name the trait
; relationship explicitly since it isn't nested inside either type's own
; definition (Convention B in extractor.py's inherit-edge handling).

(struct_item name: (type_identifier) @struct.name) @struct.definition
(enum_item name: (type_identifier) @enum.name) @enum.definition
(trait_item name: (type_identifier) @interface.name) @interface.definition

(field_declaration name: (field_identifier) @field.name) @field.definition

; Top-level functions and trait methods nest structurally (source_file /
; trait_item are themselves captured), so the ordinary parent walk finds
; their container; impl-block methods do not (see file header) and use
; ``function.parent_name`` instead. Each location is listed explicitly so
; a function_item is never captured twice.
(source_file (function_item name: (identifier) @function.name) @function.definition)
(trait_item
  body: (declaration_list
    (function_item name: (identifier) @function.name) @function.definition))
(trait_item
  body: (declaration_list
    (function_signature_item name: (identifier) @function.name) @function.definition))

(impl_item
  type: (type_identifier) @function.parent_name
  body: (declaration_list
    (function_item name: (identifier) @function.name) @function.definition))

(impl_item
  trait: (type_identifier) @implements.object
  type: (type_identifier) @implements.subject)

(use_declaration argument: (scoped_identifier) @import.module) @import.statement
(use_declaration
  argument: (use_as_clause path: (scoped_identifier) @import.module)) @import.statement

(call_expression function: [(identifier) (field_expression) (scoped_identifier)] @call.callee) @call.expression
(macro_invocation macro: (identifier) @call.callee) @call.expression
