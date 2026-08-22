use tree_sitter::{Node, Parser};

use super::error::{FfiError, FfiResult};
use super::types::{
    CDeclarations, CField, CFunction, CGlobalVar, CParam, CQualifier, CStruct, CType, CTypedef,
    CUnion, SkippedNode, c_type_from_name,
};

const ANONYMOUS_STRUCT: &str = "/* anonymous struct */";
const ANONYMOUS_UNION: &str = "/* anonymous union */";
const ANONYMOUS_ENUM: &str = "/* anonymous enum */";

/// Parse a preprocessed C header string and extract declarations.
///
/// # Errors
///
/// Returns `FfiError::Parse` if the C language cannot be initialized
/// or if the source cannot be parsed.
pub fn parse_c_header(source: &str) -> FfiResult<CDeclarations> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .map_err(|e| FfiError::Parse(format!("Failed to set C language: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| FfiError::Parse("Failed to parse C source".to_string()))?;

    let root = tree.root_node();

    // Track whether any per-child parse error was recorded so the fallback warning
    // below only fires for error shapes the per-child loop cannot see.
    let mut recorded_skip = false;

    let source_bytes = source.as_bytes();
    let mut decls = CDeclarations::default();

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.has_error() {
            let pos = child.start_position();
            let line = pos.row + 1;
            let column = pos.column + 1;
            log::warn!(
                "Skipping C declaration at line {} column {} (node kind: {}) due to parse errors",
                line,
                column,
                child.kind()
            );
            decls.skipped_nodes.push(SkippedNode {
                line,
                column,
                kind: child.kind().to_string(),
            });
            recorded_skip = true;
            continue;
        }
        match child.kind() {
            "function_definition" => {
                if let Some(func) = parse_function_sig(&child, source_bytes) {
                    decls.functions.push(func);
                }
            }
            "declaration" => {
                if has_function_declarator_child(&child) {
                    if let Some(func) = parse_function_sig(&child, source_bytes) {
                        decls.functions.push(func);
                    }
                } else if let Some(globals) = parse_global_variables(&child, source_bytes) {
                    decls.globals.extend(globals);
                }
            }
            "type_definition" => {
                let typedef = parse_typedef(&child, source_bytes);
                if let Some(td) = &typedef {
                    decls.typedefs.push(td.clone());
                }

                // Also extract struct/union/enum definitions embedded in typedefs
                let alias = typedef.as_ref().map(|td| td.name.as_str());

                if let Some(s) = extract_struct_from_type_def(&child, source_bytes, alias) {
                    decls.structs.push(s);
                }

                if let Some(u) = extract_union_from_type_def(&child, source_bytes, alias) {
                    decls.unions.push(u);
                }
            }
            _ => {}
        }
    }

    // Fallback: tree-sitter-c may group broken declarations under a root `ERROR` node.
    //
    // Because the children of that node might not individually report errors (e.g., an unclosed
    // parenthesis), we must check `root.has_error()` to detect the failure. This ensures we warn
    // the the user instead of skipping declarations silently.
    if root.has_error() && !recorded_skip {
        log::warn!(
            "C header parse tree contains errors; some declarations may be skipped (node: {})",
            root.kind()
        );
    }

    Ok(decls)
}

/// Parse a function signature from either a `function_definition` or `declaration` node.
/// Handles both `int foo(int x)` and `int *foo(int x)` forms.
fn parse_function_sig(node: &Node, source: &[u8]) -> Option<CFunction> {
    let mut cursor = node.walk();
    let mut return_type: Option<CType> = None;
    let mut return_qualifiers: Vec<CQualifier> = Vec::new();
    let mut name: Option<String> = None;
    let mut params: Vec<CParam> = Vec::new();
    let mut is_variadic = false;
    let mut is_ptr_return = false;

    for child in node.children(&mut cursor) {
        match child.kind() {
            "type_qualifier" => {
                if let Some(q) = qualifier_from_type_qualifier(&child) {
                    return_qualifiers.push(q);
                }
            }
            "primitive_type"
            | "sized_type_specifier"
            | "type_identifier"
            | "struct_specifier"
            | "union_specifier"
            | "enum_specifier"
            | "signed"
            | "unsigned"
            | "long"
            | "short" => {
                return_type = Some(parse_type_specifier(&child, source));
            }
            "function_declarator" => {
                let (fn_name, fn_params, variadic) = parse_function_declarator(&child, source)?;
                name = Some(fn_name);
                params = fn_params;
                is_variadic = variadic;
            }
            "pointer_declarator" => {
                if let Some((fn_name, fn_params, variadic)) =
                    parse_fn_from_pointer_declarator(&child, source)
                {
                    name = Some(fn_name);
                    params = fn_params;
                    is_variadic = variadic;
                    is_ptr_return = true;
                } else {
                    let (ptr_type, ptr_name) = parse_pointer_declarator(&child, source);
                    return_type = Some(ptr_type);
                    if let Some(n) = ptr_name {
                        name = Some(n);
                    }
                }
            }
            _ => {}
        }
    }

    let base_ret = return_type.take().unwrap_or(CType::Int);
    let return_type = if is_ptr_return {
        CType::Ptr(Box::new(base_ret), return_qualifiers)
    } else {
        base_ret
    };
    let name = name?;

    Some(CFunction {
        name,
        return_type,
        params,
        is_variadic,
    })
}

/// Parse a `function_declarator` node, returning (name, params, `is_variadic`).
fn parse_function_declarator(node: &Node, source: &[u8]) -> Option<(String, Vec<CParam>, bool)> {
    let mut cursor = node.walk();
    let mut name: Option<String> = None;
    let mut params: Vec<CParam> = Vec::new();
    let mut is_variadic = false;

    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                name = Some(node_text(&child, source));
            }
            "parameter_list" => {
                let (parsed_params, variadic) = parse_param_list(&child, source);
                params = parsed_params;
                is_variadic = variadic;
            }
            "pointer_declarator" => {
                if let Some((fn_ptr_name, fn_ptr_params)) = parse_fn_ptr_params(&child, source) {
                    name = Some(fn_ptr_name);
                    params.extend(fn_ptr_params);
                }
            }
            _ => {}
        }
    }

    let name = name?;
    Some((name, params, is_variadic))
}

/// Parse a `parameter_declaration` node.
fn parse_param_declaration(node: &Node, source: &[u8]) -> Option<CParam> {
    let mut cursor = node.walk();
    let mut param_type: Option<CType> = None;
    let mut param_name: Option<String> = None;
    let mut param_qualifiers: Vec<CQualifier> = Vec::new();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "type_qualifier" => {
                if let Some(q) = qualifier_from_type_qualifier(&child) {
                    param_qualifiers.push(q);
                }
            }
            "primitive_type"
            | "sized_type_specifier"
            | "type_identifier"
            | "struct_specifier"
            | "enum_specifier"
            | "signed"
            | "unsigned"
            | "long"
            | "short" => {
                param_type = Some(parse_type_specifier(&child, source));
            }
            "identifier" => {
                param_name = Some(node_text(&child, source));
            }
            "pointer_declarator" | "abstract_pointer_declarator" => {
                let (depth, ptr_qualifiers, ptr_name) = collect_ptr_qualifiers(&child, source);
                let base = param_type.take().unwrap_or(CType::Int);
                let all_qualifiers: Vec<CQualifier> = param_qualifiers
                    .iter()
                    .chain(ptr_qualifiers.iter())
                    .cloned()
                    .collect();
                // Wrap the base type once per pointer level so `char **` (named or
                // abstract) becomes `Ptr(Ptr(Char))` rather than a single `Ptr(Char)`.
                let mut ty = base;
                for _ in 0..depth {
                    ty = CType::Ptr(Box::new(ty), all_qualifiers.clone());
                }
                param_type = Some(ty);
                param_name = ptr_name;
            }
            "function_declarator" => {
                let ptype = CType::Named("/* fn ptr */".to_string());
                param_type = Some(ptype);
                if let Some(ident) = find_child(&child, "identifier") {
                    param_name = Some(node_text(&ident, source));
                }
            }
            "array_declarator" => {
                let (elem_type, _) = parse_array_declarator(&child, source);
                param_type = Some(elem_type);
            }
            _ => {}
        }
    }

    let param_type = param_type.unwrap_or(CType::Int);
    Some(CParam {
        name: param_name,
        ty: param_type,
    })
}

/// Recursively collects the pointer depth, type qualifiers, and the identifier name
/// from a declarator node.
///
/// Handles:
///
/// - named declarators (`pointer_declarator`, e.g. `char *x`)
/// - unnamed abstract declarators (`abstract_pointer_declarator`, e.g. `char *` with no name).
///   which tree-sitter-c emits for things like `int puts(const char *)`
///
/// Recurses into nested pointer declarators so multi-level pointers (`char **`) report their
/// full depth. Does not compute the pointed-to type; the caller provides the base type.
///
/// The returned depth counts this node as one level of indirection plus any nested
/// pointer declarators, so a direct call on a `pointer_declarator`/`abstract_pointer_declarator`
/// yields the total number of `*` in the chain.
fn collect_ptr_qualifiers(node: &Node, source: &[u8]) -> (usize, Vec<CQualifier>, Option<String>) {
    let mut qualifiers: Vec<CQualifier> = Vec::new();
    let mut name: Option<String> = None;
    let mut nested_depth: usize = 0;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "type_qualifier" => {
                if let Some(q) = qualifier_from_type_qualifier(&child) {
                    qualifiers.push(q);
                }
            }
            "identifier" => {
                name = Some(node_text(&child, source));
            }
            "pointer_declarator" | "abstract_pointer_declarator" => {
                // The nested declarator already counts itself as one level,
                // so we only add its result here rather than counting it again.
                let (inner_depth, inner_q, inner_name) = collect_ptr_qualifiers(&child, source);
                nested_depth += inner_depth;
                qualifiers.extend(inner_q);
                if inner_name.is_some() {
                    name = inner_name;
                }
            }
            _ => {}
        }
    }

    // This node itself is one level of indirection (named or abstract).
    let depth = 1 + nested_depth;
    (depth, qualifiers, name)
}

/// Parse a `pointer_declarator` node, returning (type, optional name).
fn parse_pointer_declarator(node: &Node, source: &[u8]) -> (CType, Option<String>) {
    let mut qualifiers: Vec<CQualifier> = Vec::new();
    let mut inner_type: Option<CType> = None;
    let mut name: Option<String> = None;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "type_qualifier" => {
                if let Some(q) = qualifier_from_type_qualifier(&child) {
                    qualifiers.push(q);
                }
            }
            "identifier" => {
                name = Some(node_text(&child, source));
            }
            "pointer_declarator" => {
                let (nested_ty, nested_name) = parse_pointer_declarator(&child, source);
                inner_type = Some(nested_ty);
                if nested_name.is_some() {
                    name = nested_name;
                }
            }
            "function_declarator" => {
                if let Some(ident) = find_child(&child, "identifier") {
                    name = Some(node_text(&ident, source));
                }
                inner_type = Some(CType::Named("/* fn */".to_string()));
            }
            "primitive_type" | "sized_type_specifier" | "type_identifier" => {
                inner_type = Some(parse_type_specifier(&child, source));
            }
            _ => {}
        }
    }

    let inner = inner_type.unwrap_or(CType::Int);
    (CType::Ptr(Box::new(inner), qualifiers), name)
}

/// Parse an `array_declarator` node, returning (`element_type`, size).
fn parse_array_declarator(node: &Node, source: &[u8]) -> (CType, Option<usize>) {
    let mut elem_type = CType::Int;
    let mut size: Option<usize> = None;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "number_literal" => {
                let text = node_text(&child, source);
                size = text.parse::<usize>().ok();
            }
            "primitive_type" | "sized_type_specifier" | "type_identifier" => {
                elem_type = parse_type_specifier(&child, source);
            }
            _ => {}
        }
    }

    (
        CType::Array {
            element_type: Box::new(elem_type),
            size,
        },
        size,
    )
}

/// Parse a type specifier and return the `CType`.
fn parse_type_specifier(node: &Node, source: &[u8]) -> CType {
    let text = node_text(node, source);

    if let Some(known) = c_type_from_name(&text) {
        return known;
    }

    match node.kind() {
        "struct_specifier" | "union_specifier" | "enum_specifier" => {
            if let Some(name_node) = find_child(node, "type_identifier") {
                let name = node_text(&name_node, source);
                CType::Named(name)
            } else {
                // Anonymous structs/unions/enums are named by their surrounding typedef
                let anon_name = match node.kind() {
                    "struct_specifier" => ANONYMOUS_STRUCT,
                    "union_specifier" => ANONYMOUS_UNION,
                    "enum_specifier" => ANONYMOUS_ENUM,
                    _ => unreachable!(),
                };
                CType::Named(anon_name.to_string())
            }
        }

        _ => CType::Named(text),
    }
}

/// Parse a typedef declaration.
fn parse_typedef(node: &Node, source: &[u8]) -> Option<CTypedef> {
    let children: Vec<Node> = {
        let mut cursor = node.walk();
        node.children(&mut cursor).collect()
    };

    let mut underlying_type: Option<CType> = None;
    let mut alias_name: Option<String> = None;
    // Track the index of the alias candidate so we can skip it in the underlying-type pass.
    // tree-sitter-c may parse known typedef names (e.g. `size_t`) as `primitive_type` rather than
    // `type_identifier`.
    let mut alias_idx: Option<usize> = None;

    for (i, child) in children.iter().enumerate() {
        let k = child.kind();
        if k == "type_identifier"
            || k == "identifier"
            || k == "primitive_type"
            || k == "sized_type_specifier"
        {
            alias_name = Some(node_text(child, source));
            alias_idx = Some(i);
        }
    }

    for (i, child) in children.iter().enumerate() {
        if Some(i) == alias_idx {
            continue;
        }
        let k = child.kind();
        match k {
            "struct_specifier" | "union_specifier" | "enum_specifier" => {
                underlying_type = Some(parse_type_specifier(child, source));
            }
            "primitive_type" | "sized_type_specifier" => {
                underlying_type = Some(parse_type_specifier(child, source));
            }
            "pointer_declarator" => {
                let (ptr_type, _) = parse_pointer_declarator(child, source);
                underlying_type = Some(ptr_type);
            }
            "function_declarator" => {
                underlying_type = Some(CType::Named("/* fn ptr */".to_string()));
            }
            _ => {}
        }
    }

    let alias = alias_name?;
    let underlying = match underlying_type.unwrap_or(CType::Int) {
        CType::Named(name)
            if name == ANONYMOUS_STRUCT || name == ANONYMOUS_UNION || name == ANONYMOUS_ENUM =>
        {
            CType::Named(alias.clone())
        }
        underlying => underlying,
    };

    if alias == underlying.to_string() && matches!(underlying, CType::Int) {
        return None;
    }

    Some(CTypedef {
        name: alias,
        underlying,
    })
}

/// Parse global variable declarations from a declaration node.
fn parse_global_variables(node: &Node, source: &[u8]) -> Option<Vec<CGlobalVar>> {
    let mut cursor = node.walk();
    let mut base_type: Option<CType> = None;
    let mut vars: Vec<CGlobalVar> = Vec::new();
    let mut is_const = false;

    for child in node.children(&mut cursor) {
        match child.kind() {
            "type_qualifier" => {
                if child.child(0).is_some_and(|c| c.kind() == "const") {
                    is_const = true;
                }
            }
            "primitive_type"
            | "sized_type_specifier"
            | "type_identifier"
            | "struct_specifier"
            | "enum_specifier"
            | "signed"
            | "unsigned"
            | "long"
            | "short" => {
                base_type = Some(parse_type_specifier(&child, source));
            }
            "init_declarator" => {
                if let Some(var) = parse_init_declarator(&child, source, &base_type, is_const) {
                    vars.push(var);
                }
            }
            "pointer_declarator" => {
                let (ptr_type, name) = parse_pointer_declarator(&child, source);
                if let Some(name) = name {
                    vars.push(CGlobalVar {
                        name,
                        ty: ptr_type,
                        is_const,
                    });
                }
            }
            "identifier" => {
                let name = node_text(&child, source);
                let ty = base_type.clone().unwrap_or(CType::Int);
                vars.push(CGlobalVar { name, ty, is_const });
            }
            _ => {}
        }
    }

    if vars.is_empty() { None } else { Some(vars) }
}

/// Parse an `init_declarator` (variable with optional initializer).
fn parse_init_declarator(
    node: &Node,
    source: &[u8],
    base_type: &Option<CType>,
    is_const: bool,
) -> Option<CGlobalVar> {
    let mut cursor = node.walk();
    let mut name: Option<String> = None;
    let mut var_type: Option<CType> = None;

    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                name = Some(node_text(&child, source));
                var_type.clone_from(base_type);
            }
            "pointer_declarator" => {
                let (ptr_type, ptr_name) = parse_pointer_declarator(&child, source);
                name = ptr_name;
                var_type = Some(ptr_type);
            }
            "array_declarator" => {
                let (arr_type, _) = parse_array_declarator(&child, source);
                if let Some(ident) = find_child(&child, "identifier") {
                    name = Some(node_text(&ident, source));
                }
                var_type = Some(arr_type);
            }
            _ => {}
        }
    }

    let var_type = var_type.or_else(|| base_type.clone())?;
    let name = name?;

    Some(CGlobalVar {
        name,
        ty: var_type,
        is_const,
    })
}

/// Extract a struct definition from inside a `type_definition` node (e.g. `typedef struct X { ... } X;`).
fn extract_struct_from_type_def(
    node: &Node,
    source: &[u8],
    fallback_name: Option<&str>,
) -> Option<CStruct> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "struct_specifier" {
            return parse_struct_specifier(&child, source, fallback_name);
        }
    }
    None
}

/// Extract a union definition from inside a `type_definition` node.
fn extract_union_from_type_def(
    node: &Node,
    source: &[u8],
    fallback_name: Option<&str>,
) -> Option<CUnion> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "union_specifier"
            && let Some(s) = parse_struct_specifier(&child, source, fallback_name)
        {
            return Some(CUnion {
                name: s.name,
                fields: s.fields,
            });
        }
    }
    None
}

/// Parse a `struct_specifier` node into a `CStruct`.
fn parse_struct_specifier(
    node: &Node,
    source: &[u8],
    fallback_name: Option<&str>,
) -> Option<CStruct> {
    let mut cursor = node.walk();
    let mut name = String::new();
    let mut fields: Vec<CField> = Vec::new();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "type_identifier" => {
                name = node_text(&child, source);
            }
            "field_declaration_list" => {
                if let Some(fs) = parse_field_declaration_list(&child, source) {
                    fields = fs;
                }
            }
            _ => {}
        }
    }

    if name.is_empty() {
        name = fallback_name?.to_string();
    }
    Some(CStruct { name, fields })
}

/// Parse a `field_declaration_list` node into a Vec of `CField`.
fn parse_field_declaration_list(node: &Node, source: &[u8]) -> Option<Vec<CField>> {
    let mut cursor = node.walk();
    let mut fields = Vec::new();
    for child in node.children(&mut cursor) {
        if child.kind() == "field_declaration"
            && let Some((name, ty)) = parse_field_declaration(&child, source)
        {
            fields.push(CField {
                name: Some(name),
                ty,
            });
        }
    }
    if fields.is_empty() {
        None
    } else {
        Some(fields)
    }
}

/// Parse a `field_declaration` node, returning (`field_name`, `field_type`).
fn parse_field_declaration(node: &Node, source: &[u8]) -> Option<(String, CType)> {
    let mut cursor = node.walk();
    let mut field_type: Option<CType> = None;
    let mut field_name: Option<String> = None;

    for child in node.children(&mut cursor) {
        match child.kind() {
            "type_qualifier" => {
                // const/volatile on fields: note but skip for now
            }
            "primitive_type" | "sized_type_specifier" | "type_identifier" => {
                field_type = Some(parse_type_specifier(&child, source));
            }
            "field_identifier" => {
                field_name = Some(node_text(&child, source));
            }
            "pointer_declarator" => {
                let (ptr_type, ptr_name) = parse_pointer_declarator(&child, source);
                field_type = Some(ptr_type);
                field_name = ptr_name;
            }
            _ => {}
        }
    }

    let name = field_name?;
    let ty = field_type.unwrap_or(CType::Int);
    Some((name, ty))
}

/// Get the text of a tree-sitter node.
fn node_text(node: &Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or_default().to_string()
}

/// Find a child node by kind (direct children only).
fn find_child<'a>(node: &'a Node, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|c| c.kind() == kind)
}

/// Find a child node by kind, unwrapping any `declarator` wrappers.
///
/// tree-sitter-c wraps declarators in `declarator` nodes, so
/// `pointer_declarator -> declarator -> function_declarator` needs this unwrapping.
fn find_declarator_child<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k == kind {
            return Some(child);
        }
        if k == "declarator"
            && let Some(found) = find_declarator_child(child, kind)
        {
            return Some(found);
        }
    }
    None
}

/// Check whether a declaration node contains a function declarator (possibly
/// wrapped in `pointer_declarator` layers), distinguishing function declarations
/// from variable declarations.
fn has_function_declarator_child(node: &Node) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| {
        let k = c.kind();
        k == "function_declarator"
            || (k == "pointer_declarator"
                && find_declarator_child(c, "function_declarator").is_some())
    })
}

/// Extract a `CQualifier` from a `type_qualifier` tree-sitter node (which wraps
/// `const`/`volatile`/`restrict` as its first child).
fn qualifier_from_type_qualifier(node: &Node) -> Option<CQualifier> {
    let inner = node.child(0)?;
    match inner.kind() {
        "const" => Some(CQualifier::Const),
        "volatile" => Some(CQualifier::Volatile),
        "restrict" => Some(CQualifier::Restrict),
        _ => None,
    }
}

/// Parse a function from a `pointer_declarator` that wraps a `function_declarator`
/// (e.g. `int *foo(int)` where `*foo(int)` is the `pointer_declarator`).
/// Returns `None` if the `pointer_declarator` does not contain a `function_declarator`
/// (e.g. it's just a value pointer like `int *x`).
fn parse_fn_from_pointer_declarator(
    node: &Node,
    source: &[u8],
) -> Option<(String, Vec<CParam>, bool)> {
    let fn_decl = find_declarator_child(*node, "function_declarator")?;
    parse_function_declarator(&fn_decl, source)
}

/// Parse a parameter declaration, skipping C `void` params (which mean "no parameters"
/// per C convention).
fn parse_non_void_param(node: &Node, source: &[u8]) -> Option<CParam> {
    let param = parse_param_declaration(node, source)?;
    if param.ty == CType::Void {
        None
    } else {
        Some(param)
    }
}

/// Extract the identifier name and parameter list from a function-pointer
/// `pointer_declarator` like `(*cb)(int, float)`.
fn parse_fn_ptr_params(node: &Node, source: &[u8]) -> Option<(String, Vec<CParam>)> {
    let inner_fn = find_declarator_child(*node, "function_declarator")?;
    let ptr_name = find_declarator_child(*node, "identifier").map(|n| node_text(&n, source));
    let mut params = Vec::new();
    if let Some(inner_params) = find_child(&inner_fn, "parameter_list") {
        let mut c = inner_params.walk();
        for p in inner_params.children(&mut c) {
            if p.kind() == "parameter_declaration"
                && let Some(param) = parse_non_void_param(&p, source)
            {
                params.push(param);
            }
        }
    }
    Some((ptr_name?, params))
}

/// Parse a C parameter list node, extracting parameters and variadic flag.
fn parse_param_list(node: &Node, source: &[u8]) -> (Vec<CParam>, bool) {
    let mut cursor = node.walk();
    let mut params = Vec::new();
    let mut is_variadic = false;
    for param_node in node.children(&mut cursor) {
        match param_node.kind() {
            "parameter_declaration" => {
                if let Some(param) = parse_non_void_param(&param_node, source) {
                    params.push(param);
                }
            }
            "..." | "variadic_parameter" => {
                is_variadic = true;
            }
            _ => {}
        }
    }
    (params, is_variadic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// **Regression test** (tree-sitter-c).
    ///
    /// An `abstract_pointer_declarator` for an unnamed pointer parameter (e.g. `int puts(const char *)`).
    ///
    /// Previously, the `*` was silently ignored, returning `Char` instead of `Ptr(Char, [Const])`,
    /// which broke `CString` handling on platforms where their headers omit param names (i.e., `stdio.h`
    /// on macOS).
    fn test_unnamed_pointer_parameter_is_not_dropped() {
        let src = "int puts(const char *); \
        char *gets(char *); \
        int fopen(const char *__filename, const char *__mode);";

        let decls = parse_c_header(src).unwrap();

        let puts = decls.functions.iter().find(|f| f.name == "puts").unwrap();
        assert_eq!(puts.params.len(), 1);
        assert_eq!(
            puts.params[0].ty,
            CType::Ptr(Box::new(CType::Char), vec![CQualifier::Const])
        );

        let gets = decls.functions.iter().find(|f| f.name == "gets").unwrap();
        assert_eq!(gets.params.len(), 1);
        assert_eq!(gets.params[0].ty, CType::Ptr(Box::new(CType::Char), vec![]));

        let fopen = decls.functions.iter().find(|f| f.name == "fopen").unwrap();
        assert_eq!(
            fopen.params[0].ty,
            CType::Ptr(Box::new(CType::Char), vec![CQualifier::Const])
        );
        assert_eq!(
            fopen.params[1].ty,
            CType::Ptr(Box::new(CType::Char), vec![CQualifier::Const])
        );
    }
}
