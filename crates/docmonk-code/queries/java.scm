; Java query file -- see python.scm for the shared capture-name contract.
; Annotations nest inside the ``modifiers`` child of the declaration they
; annotate, so (unlike Python/TS decorators) the ordinary structural walk
; already finds the right subject; ``decorator.subject`` is bound
; explicitly anyway so extractor.py's correlation code stays uniform
; across languages instead of special-casing "already nested".

(package_declaration (scoped_identifier) @namespace.name) @namespace.definition

(class_declaration name: (identifier) @class.name) @class.definition
(interface_declaration name: (identifier) @interface.name) @interface.definition
(enum_declaration name: (identifier) @enum.name) @enum.definition

(superclass (type_identifier) @extends.object)
(super_interfaces (type_list (type_identifier) @implements.object))

(method_declaration name: (identifier) @function.name) @function.definition
(constructor_declaration name: (identifier) @function.name) @function.definition

(field_declaration
  declarator: (variable_declarator name: (identifier) @field.name)) @field.definition

(import_declaration (scoped_identifier) @import.module) @import.statement

(method_invocation name: (identifier) @call.callee) @call.expression
(object_creation_expression type: (type_identifier) @call.callee) @call.expression

(method_declaration
  (modifiers
    [
      (annotation) @decorator.raw
      (marker_annotation) @decorator.raw
    ])) @decorator.subject
