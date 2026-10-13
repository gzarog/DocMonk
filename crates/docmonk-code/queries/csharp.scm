; C# query file -- see python.scm for the shared capture-name contract.
;
; C#'s ``base_list`` (``class Dog : Animal, IBarks``) does not distinguish
; a base class from implemented interfaces at the grammar level -- that is
; a semantic fact (a class has at most one base class, always listed
; first) this generic AST-only extractor does not attempt to reconstruct.
; Every base_list entry is conservatively tagged EXTENDS; a naming-
; convention guess (leading ``I`` => interface) would be exactly the kind
; of non-generic heuristic framework_rules.py exists for, not extractor.py.

(namespace_declaration name: (identifier) @namespace.name) @namespace.definition
(namespace_declaration name: (qualified_name) @namespace.name) @namespace.definition

(class_declaration name: (identifier) @class.name) @class.definition
(interface_declaration name: (identifier) @interface.name) @interface.definition
(struct_declaration name: (identifier) @struct.name) @struct.definition
(enum_declaration name: (identifier) @enum.name) @enum.definition

(base_list (identifier) @extends.object)

(method_declaration name: (identifier) @function.name) @function.definition
(constructor_declaration name: (identifier) @function.name) @function.definition

(property_declaration name: (identifier) @property.name) @property.definition
(field_declaration
  (variable_declaration
    (variable_declarator name: (identifier) @field.name))) @field.definition

(using_directive [(identifier) (qualified_name)] @import.module) @import.statement

(invocation_expression function: [(identifier) (member_access_expression)] @call.callee) @call.expression
(object_creation_expression type: (identifier) @call.callee) @call.expression

(method_declaration
  (attribute_list
    (attribute) @decorator.raw)) @decorator.subject
