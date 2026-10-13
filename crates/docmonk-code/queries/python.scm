; Python query file for code/extractor.py.
;
; Capture-name contract (shared across all languages, see extractor.py):
;   <kind>.definition / <kind>.name   -- an entity, kind in
;       {class, function, field}. Python has no separate interface/struct/
;       enum keyword so those kinds are simply never emitted here.
;   extends.object                    -- a base-class name, nested inside
;       the class it belongs to so extractor.py's structural walk finds the
;       enclosing class.definition as the relationship subject.
;   import.module                     -- the dotted module path of an
;       import statement; import.alias is its optional "as" binding.
;   call.expression / call.callee     -- a call site and its callee text
;       (may be a dotted attribute expression, e.g. "self.bar").
;   decorator.raw                     -- a decorator attached to the
;       function/class it co-occurs with in the same query match.

(class_definition
  name: (identifier) @class.name
  superclasses: (argument_list (identifier) @extends.object)?) @class.definition

(function_definition
  name: (identifier) @function.name) @function.definition

; Direct class-body assignments only: nesting the pattern inside the
; class's own (block) means it cannot match assignments inside a nested
; method body, since that lives in a different (block) node.
;
; RagMonk Rust: the crates.io Python grammar keeps the (expression_statement)
; wrapper that the reference's bundled grammar inlines, so the assignment is
; matched one level down; the captured nodes are identical.
(class_definition
  body: (block
    (expression_statement
      (assignment left: (identifier) @field.name) @field.definition)))

(import_statement name: (dotted_name) @import.module) @import.statement
(import_statement
  name: (aliased_import
    name: (dotted_name) @import.module
    alias: (identifier) @import.alias)) @import.statement
(import_from_statement module_name: (dotted_name) @import.module) @import.statement

(call
  function: [(identifier) (attribute)] @call.callee) @call.expression

(decorated_definition
  (decorator) @decorator.raw
  definition: (_) @decorator.subject)
