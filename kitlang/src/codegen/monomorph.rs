//! Monomorphization of generic (template) definitions.
//!
//! A generic application with fully concrete type arguments is resolved directly
//! to its monomorph: the template is cloned, its type parameters are
//! substituted, and the result is registered under the deterministic name
//! produced by `monomorph_name`.
//!
//! An application with unresolved arguments gets a fresh *instance type
//! variable* recorded in `MonomorphState::instance_types`; the variable is
//! bound by ordinary unification and resolved at the end of each pass by
//! `generate_monomorphs`, which realizes the monomorph, binds the instance
//! variable to `Named(mono_name)`, and rewrites generic-function call sites.
//! Monomorphs are staged into the merged `Program` so the next inference pass
//! types them, letting transitive generics cascade through the fixpoint driver.

use std::collections::HashMap;

use crate::codegen::hash;
use crate::error::{CompilationError, CompileResult};
use crate::type_err;

use super::ast::{DefaultSpecialization, Expr, ExprKind, Function, Param, Program, Stmt, StmtKind};
use super::inference::TypeInferencer;
use super::module::ModulePath;
use super::type_ast::{EnumDefinition, StructDefinition, TraitDefinition, TypeParam};
use super::types::{ToCRepr, Type, TypeId};

#[derive(Clone)]
/// A stashed generic (template) declaration. Templates are never typed or
/// emitted directly; monomorphization clones them and substitutes their
/// type parameters with concrete types.
pub(crate) enum TemplateDef {
    Struct(StructDefinition),
    Enum(EnumDefinition),
    Function(Function),
}

impl TemplateDef {
    /// The declared type parameters of this template.
    fn type_params(&self) -> &[TypeParam] {
        match self {
            TemplateDef::Struct(s) => &s.type_params,
            TemplateDef::Enum(e) => &e.type_params,
            TemplateDef::Function(f) => &f.type_params,
        }
    }
}

/// A generic application whose type arguments are not all concrete:
/// an instance type variable mapped to the application it stands for.
#[derive(Clone)]
pub(crate) struct PendingInstance {
    pub base: String,
    pub params: Vec<TypeId>,
}

/// A generic application recorded during inference and resolved at the end of
/// the pass. `call_return`, when set, identifies the function call site that
/// must be rewritten to the monomorph's name once the application resolves.
pub(crate) struct PendingGeneric {
    pub base: String,
    pub params: Vec<TypeId>,
    pub call_return: Option<TypeId>,
}

/// A registered trait implementation: the trait satisfied and the concrete type it is for.
///
/// Stored in `MonomorphState::impls` keyed by the `for_type`'s canonical C name. `methods` carries
/// the impl's function bodies; constraint checking resolves through `lookup_impl`, method calls
/// resolve to these methods' mangled symbols via `lookup_method`.
#[derive(Clone)]
#[allow(dead_code)] // read back via `validate_trait_impls`; dispatch consumes only the symbol.
pub(crate) struct ImplEntry {
    pub trait_name: String,
    pub for_type: Type,
    pub methods: Vec<Function>,
}

/// A trait implementation whose `for_type` is itself a generic application, e.g.
/// `implement Foo for WrapperType[T]`. The methods still reference the type parameter `T`, so the
/// impl cannot be registered under a concrete key yet. It is realized into a concrete `ImplEntry`
/// (with `T` substituted) once a monomorph such as `WrapperType[Int]` is realized, via
/// `realize_template_impls`. Without this two-step registration, generic `for_type` impls would sit
/// under an unmatchable template key and every call on the concrete type would miss them.
#[derive(Clone)]
pub(crate) struct TemplateImpl {
    pub trait_name: String,
    pub for_type: Type,
    pub methods: Vec<Function>,
    pub module_path: ModulePath,
}

/// Monomorphization state owned by the type inferencer.
#[derive(Default)]
pub(crate) struct MonomorphState {
    /// Stashed templates keyed by declared name, together with the module
    /// that defines them.
    pub(crate) templates: HashMap<String, (ModulePath, TemplateDef)>,
    /// Default specializations (`default Trait as Type`) gathered across modules. Used by
    /// `instance_params_and_type` to resolve missing type arguments constrained by a trait.
    pub(crate) defaults: Vec<DefaultSpecialization>,
    /// Instance type variables of generic applications with unresolved args.
    pub(crate) instance_types: HashMap<TypeId, PendingInstance>,
    /// Generic applications created during the current inference pass.
    pub(crate) pending: Vec<PendingGeneric>,
    /// Cache of realized monomorphs: `(template, concrete args)` -> name.
    pub(crate) complete: HashMap<(String, Vec<Type>), String>,
    /// Monomorph name -> module that defines its template (for codegen mangling).
    pub(crate) mono_modules: HashMap<String, ModulePath>,
    /// Monomorph name -> (template name, concrete args) (for unification and codegen).
    pub(crate) mono_instances: HashMap<String, (String, Vec<Type>)>,
    /// Monomorph declarations staged this pass; drained into the merged program
    /// by `generate_monomorphs`.
    pub(crate) staged_structs: Vec<StructDefinition>,
    pub(crate) staged_enums: Vec<EnumDefinition>,
    pub(crate) staged_functions: Vec<Function>,
    /// Impl methods realized from template impls (`implement Foo for WrapperType[T]`) per concrete
    /// monomorph. Drained into the merged program by `generate_monomorphs` so the next pass infers
    /// and emits them.
    pub(crate) staged_impl_methods: Vec<Function>,
    /// Applications whose parameters could not be resolved this pass,
    /// recorded as (template name, first unresolved parameter name).
    pub(crate) unresolved: Vec<(String, String)>,
    /// Trait implementations, keyed by the `for_type`'s canonical C name
    /// (`to_c_repr().name`). Within each bucket, entries are also tagged with
    /// their `trait_name`. Populated once before the fixpoint loop; this is the
    /// shared lookup table used both for constraint checking and for
    /// method dispatch, so its key convention is what keeps the two consistent.
    pub(crate) impls: HashMap<String, Vec<ImplEntry>>,
    /// Trait implementations whose `for_type` is a generic application (e.g.
    /// `implement Foo for WrapperType[T]`). Stored unsubstituted until a matching concrete
    /// monomorph is realized, at which point `realize_template_impls` instantiates them.
    /// Registering them directly would leave them under an unmatchable template key.
    pub(crate) template_impls: Vec<TemplateImpl>,
    /// Prepared impl methods ready for inference and C emission. Each method is
    /// given a stable mangled name (`<trait>__<forType>__<method>`), has a
    /// synthesized `this` parameter prepended, and is associated with its
    /// declaring module via `mono_modules` so per-module codegen emits it under
    /// the same mangled name the call site rewrites to (no `#[expose]`: the
    /// name agreement comes from module mangling, not from suppressing it).
    /// Built in `register_impls`.
    pub(crate) impl_methods: Vec<Function>,
}

/// Deterministic unique name for a monomorph: `<name>_<hash>` where the hash
/// covers the C names of the type arguments, so identical applications share
/// one C symbol (mirrors `monomorphName`/`hashParams` in the reference).
pub(crate) fn monomorph_name(base: &str, args: &[Type]) -> String {
    let signature: Vec<String> = args.iter().map(|t| t.to_c_repr().name).collect();
    let key = format!("{}|{}", base, signature.join("|"));
    format!("{}_{}", base, hash::djb2_str(&key))
}

/// Extract the trait name from a constraint type (e.g. `Hashable` from a `T: Hashable` bound).
///
/// Trait constraints are stored as plain trait references (`Type::Named` or `Type::Instance`); only
/// these simple forms are supported. Parameterized / associated-type constraints are deferred, so
/// this returns `None` for anything else (the impl is skipped rather than silently mis-checked).
pub(crate) fn constraint_trait_name(constraint: &Type) -> Option<String> {
    match constraint {
        Type::Named(name) => Some(name.clone()),
        Type::Instance { base, .. } => Some(base.clone()),
        _ => None,
    }
}

/// Whether `ty` is fully concrete (contains no `TypeParam`).
///
/// Gates eager constraint checking: a partially-bound generic parameter (a fresh unknown) cannot be
/// checked until its argument resolves, so it is deferred to a later pass.
///
/// Note on `Type::Struct`: a user struct type carries its fields as `TypeId`s stored in the
/// `TypeStore`, not as inlined `Type`s, so this function cannot inspect them and treats `Struct` as
/// concrete. That is sound because `Struct` is only ever produced for fully-resolved struct types
/// (generic structs are represented as `Type::Instance`); an unresolved field type would surface as
/// a separate unresolved-type error via `validate_monomorphs`.
pub(crate) fn is_concrete(ty: &Type) -> bool {
    match ty {
        Type::TypeParam(_) => false,
        Type::Named(_)
        | Type::Void
        | Type::Int
        | Type::Float
        | Type::Bool
        | Type::Char
        | Type::CString
        | Type::Size
        | Type::Int8
        | Type::Int16
        | Type::Int32
        | Type::Int64
        | Type::Uint8
        | Type::Uint16
        | Type::Uint32
        | Type::Uint64
        | Type::Float32
        | Type::Float64
        | Type::Struct { .. } => true,
        Type::Instance { args, .. } => args.iter().all(is_concrete),
        Type::Ptr(inner) => is_concrete(inner),
        Type::Tuple(elems) => elems.iter().all(is_concrete),
        Type::CArray(elem, _) => is_concrete(elem),
        Type::Function { param_tys, ret_ty } => {
            is_concrete(ret_ty) && param_tys.iter().all(is_concrete)
        }
    }
}

/// Whether an impl `for_type` is a generic application over an unbound type, e.g.
/// `implement Foo for WrapperType[T]`. Type parameters are parsed as `Named("T")` references (not
/// `TypeParam`), so an argument is "unbound" when its name is not a known concrete template
/// (struct/enum). Such impls are deferred to `realize_template_impls`, which substitutes the
/// concrete arguments once the monomorph is realized.
fn for_type_is_templated(
    templates: &HashMap<String, (ModulePath, TemplateDef)>,
    ty: &Type,
) -> bool {
    match ty {
        Type::Instance { args, .. } => args.iter().any(|a| type_arg_is_unbound(templates, a)),
        _ => false,
    }
}

/// Whether a type argument mentions an unbound type name (a type parameter), i.e. a `Named` whose
/// name is not a known concrete template (struct/enum). Concrete arguments (primitives like `Int`,
/// and `Named` references to actual structs/enums) are not unbound.
fn type_arg_is_unbound(templates: &HashMap<String, (ModulePath, TemplateDef)>, ty: &Type) -> bool {
    match ty {
        Type::Named(name) => !templates.contains_key(name),
        Type::Instance { args, .. } => args.iter().any(|a| type_arg_is_unbound(templates, a)),
        Type::Ptr(inner) => type_arg_is_unbound(templates, inner),
        Type::Tuple(elems) => elems.iter().any(|a| type_arg_is_unbound(templates, a)),
        Type::CArray(elem, _) => type_arg_is_unbound(templates, elem),
        _ => false,
    }
}

/// Normalize a type to its concrete, codegen-ready representation.
///
/// A generic application (`Container[Int]`) resolves to the realized monomorph name
/// (`Named("Container_<hash>")`), which is exactly how a receiver of that type appears after
/// monomorphization. This is what lets an impl registered against `Container[Int]` be found for a
/// receiver whose type is the realized `Container_<hash>` monomorph.
pub(crate) fn canonical_type(ty: &Type) -> Type {
    match ty {
        Type::Instance { base, args } => Type::Named(monomorph_name(base, args)),
        other => other.clone(),
    }
}

/// Canonical C-name key for a type, used to key the impl registry and build impl-method symbols.
///
/// Primitives (`Int`) map to their C name (`int`); generic applications map to the realized
/// monomorph name; everything else to its `to_c_repr` name. Used by both `lookup_impl` and
/// `lookup_method` so constraint checking and method dispatch agree on keys.
pub(crate) fn canonical_type_key(ty: &Type) -> String {
    canonical_type(ty).to_c_repr().name
}

/// Deterministic C symbol for an impl method: `<trait>__<forType>__<method>`.
///
/// The `<forType>` segment is the canonical type key, so the symbol a call site rewrites to matches
/// the symbol `register_impls` gave the prepared method.
pub(crate) fn impl_method_symbol(trait_name: &str, for_type: &Type, method_name: &str) -> String {
    format!(
        "{}__{}__{}",
        trait_name,
        canonical_type_key(for_type),
        method_name
    )
}

/// Compare an impl method's signature against its trait declaration.
///
/// Uses the canonical C name of each parameter/return type (via `to_c_repr`) so the comparison is
/// robust to representation differences between the declaration and the impl (e.g. `Named` vs
/// `Struct` vs a realized `Instance`). Generic-method signatures (involving `TypeParam`) are
/// compared by their raw names; deeper generic-method validation is out of scope.
pub(crate) fn signatures_match(decl: &Function, provided: &Function) -> bool {
    if decl.params.len() != provided.params.len() {
        return false;
    }
    for (a, b) in decl.params.iter().zip(provided.params.iter()) {
        match (a.annotation.as_ref(), b.annotation.as_ref()) {
            (Some(x), Some(y)) if x.to_c_repr().name == y.to_c_repr().name => continue,
            _ => return false,
        }
    }
    match (decl.return_type.as_ref(), provided.return_type.as_ref()) {
        (Some(x), Some(y)) => x.to_c_repr().name == y.to_c_repr().name,
        (None, None) => true,
        _ => false,
    }
}

/// Substitute type parameters with their concrete types throughout a type tree.
fn substitute_type(ty: &Type, map: &HashMap<String, Type>) -> Type {
    match ty {
        Type::TypeParam(name) => map.get(name).cloned().unwrap_or_else(|| ty.clone()),
        Type::Instance { base, args } => Type::Instance {
            base: base.clone(),
            args: args.iter().map(|a| substitute_type(a, map)).collect(),
        },
        Type::Ptr(inner) => Type::Ptr(Box::new(substitute_type(inner, map))),
        Type::Tuple(elems) => Type::Tuple(elems.iter().map(|e| substitute_type(e, map)).collect()),
        Type::CArray(elem, size) => Type::CArray(Box::new(substitute_type(elem, map)), *size),
        Type::Function { param_tys, ret_ty } => Type::Function {
            param_tys: param_tys.iter().map(|p| substitute_type(p, map)).collect(),
            ret_ty: Box::new(substitute_type(ret_ty, map)),
        },
        _ => ty.clone(),
    }
}

/// Substitute type parameters throughout a function definition (parameters, return type, and body
/// annotations), cloning it in the process. Used when realizing a template impl method for a
/// concrete monomorph, e.g. turning `function get(): T` into `function get(): Int` for
/// `WrapperType[Int]`.
fn substitute_function(func: &Function, map: &HashMap<String, Type>) -> Function {
    let mut f = func.clone();
    for param in &mut f.params {
        param.annotation = param.annotation.as_ref().map(|a| substitute_type(a, map));
    }
    f.return_type = f.return_type.as_ref().map(|r| substitute_type(r, map));
    for stmt in &mut f.body.stmts {
        substitute_stmt_annotations(stmt, map);
    }
    f
}

/// Substitute type parameters inside the `struct <type> { ... }` annotations
/// of an expression tree (the only type-carrying part of expressions).
fn substitute_expr_annotations(expr: &mut Expr, map: &HashMap<String, Type>) {
    if let ExprKind::StructInit {
        struct_type: Some(st),
        ..
    } = &mut expr.kind
    {
        *st = substitute_type(st, map);
    }
    match &mut expr.kind {
        ExprKind::Call { callee, args } => {
            substitute_expr_annotations(callee, map);
            for a in args {
                substitute_expr_annotations(a, map);
            }
        }
        ExprKind::UnaryOp { expr, .. } => substitute_expr_annotations(expr, map),
        ExprKind::BinaryOp { left, right, .. } => {
            substitute_expr_annotations(left, map);
            substitute_expr_annotations(right, map);
        }
        ExprKind::Assign { left, right, .. } => {
            substitute_expr_annotations(left, map);
            substitute_expr_annotations(right, map);
        }
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            substitute_expr_annotations(cond, map);
            substitute_expr_annotations(then_branch, map);
            substitute_expr_annotations(else_branch, map);
        }
        ExprKind::RangeLiteral { start, end } => {
            substitute_expr_annotations(start, map);
            substitute_expr_annotations(end, map);
        }
        ExprKind::StructInit { fields, .. } => {
            for f in fields {
                substitute_expr_annotations(&mut f.value, map);
            }
        }
        ExprKind::FieldAccess { expr, .. } => substitute_expr_annotations(expr, map),
        ExprKind::Index { expr, index } => {
            substitute_expr_annotations(expr, map);
            substitute_expr_annotations(index, map);
        }
        ExprKind::EnumInit { args, .. } => {
            for a in args {
                substitute_expr_annotations(a, map);
            }
        }
        ExprKind::ArrayLiteral { elements } => {
            for e in elements {
                substitute_expr_annotations(e, map);
            }
        }
        ExprKind::TupleLit { elements } => {
            for e in elements {
                substitute_expr_annotations(e, map);
            }
        }
        ExprKind::Identifier { .. } | ExprKind::Literal { .. } | ExprKind::EnumVariant { .. } => {}
    }
}

/// Substitute type parameters inside the type annotations of a statement tree
/// (variable declaration annotations and nested `struct`-init types).
fn substitute_stmt_annotations(stmt: &mut Stmt, map: &HashMap<String, Type>) {
    match &mut stmt.kind {
        StmtKind::VarDecl {
            annotation, init, ..
        } => {
            if let Some(ann) = annotation {
                *ann = substitute_type(ann, map);
            }
            if let Some(init) = init {
                substitute_expr_annotations(init, map);
            }
        }
        StmtKind::Expr(e) => substitute_expr_annotations(e, map),
        StmtKind::Return(e) => {
            if let Some(e) = e {
                substitute_expr_annotations(e, map);
            }
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            substitute_expr_annotations(cond, map);
            for s in &mut then_branch.stmts {
                substitute_stmt_annotations(s, map);
            }
            if let Some(b) = else_branch {
                for s in &mut b.stmts {
                    substitute_stmt_annotations(s, map);
                }
            }
        }
        StmtKind::While { cond, body } => {
            substitute_expr_annotations(cond, map);
            for s in &mut body.stmts {
                substitute_stmt_annotations(s, map);
            }
        }
        StmtKind::For { iter, body, .. } => {
            substitute_expr_annotations(iter, map);
            for s in &mut body.stmts {
                substitute_stmt_annotations(s, map);
            }
        }
        StmtKind::Match(m) => {
            substitute_expr_annotations(&mut m.expr, map);
            for arm in &mut m.arms {
                for s in &mut arm.body.stmts {
                    substitute_stmt_annotations(s, map);
                }
            }
        }
        StmtKind::Defer { body } => substitute_stmt_annotations(body, map),
        StmtKind::Block(b) => {
            for s in &mut b.stmts {
                substitute_stmt_annotations(s, map);
            }
        }
        StmtKind::Break | StmtKind::Continue => {}
    }
}

impl TypeInferencer {
    /// Stash every generic declaration of `module` as a template for later monomorphization.
    ///
    /// Generic enum variants are also registered in the symbol table so constructor calls,
    /// patterns, and bare variant references can find generic enums; their argument types keep
    /// the raw annotations (type parameters), which the generic inference paths substitute per
    /// application.
    pub(crate) fn register_templates(&mut self, module_path: &ModulePath, prog: &Program) {
        self.monomorphs
            .defaults
            .extend(prog.defaults.iter().cloned());
        for s in prog.structs.iter().filter(|s| !s.type_params.is_empty()) {
            self.monomorphs.templates.insert(
                s.name.clone(),
                (module_path.clone(), TemplateDef::Struct(s.clone())),
            );
        }
        for e in prog.enums.iter().filter(|e| !e.type_params.is_empty()) {
            self.monomorphs.templates.insert(
                e.name.clone(),
                (module_path.clone(), TemplateDef::Enum(e.clone())),
            );
            let mut resolved = e.clone();
            for variant in &mut resolved.variants {
                for arg in &mut variant.args {
                    arg.ty = self
                        .store
                        .new_known(arg.annotation.clone().unwrap_or(Type::Void));
                }
            }
            for variant in &resolved.variants {
                self.symbols_mut().define_enum_variant(variant);
            }
        }
        for f in prog.functions.iter().filter(|f| !f.type_params.is_empty()) {
            self.monomorphs.templates.insert(
                f.name.clone(),
                (module_path.clone(), TemplateDef::Function(f.clone())),
            );
        }
    }

    /// Register trait implementations into the per-program impl table and prepare their methods for
    /// inference and C emission.
    ///
    /// Keyed by the `for_type`'s canonical C name; each entry also records its `trait_name`. The
    /// table is shared by constraint checking and method dispatch, so its key convention keeps the
    /// two consistent.
    ///
    /// An impl whose `for_type` is itself a generic application (e.g. `implement Foo for
    /// WrapperType[T]`) cannot be keyed to a concrete type yet, since its methods still reference
    /// the type parameter. Such impls are stashed in `template_impls` and realized per concrete
    /// monomorph by `realize_template_impls` once, say, `WrapperType[Int]` is monomorphized;
    /// registering them directly would leave them under an unmatchable template key.
    ///
    /// For each concrete impl method we also build a *prepared* `Function`: it is given the stable
    /// mangled symbol `<trait>__<forType>__<method>`, has a synthesized `this` parameter prepended
    /// (the receiver), and is associated with its declaring module via `mono_modules` so per-module
    /// codegen emits it under the same mangled name the call site rewrites to (no `#[expose]`: the
    /// name agreement comes from module mangling). The signature is pre-registered in the symbol
    /// table so calls resolve before the body is inferred. Prepared functions are collected in
    /// `impl_methods`; the driver appends them to the merged program for inference and emission.
    pub(crate) fn register_impls(
        &mut self,
        module_path: &ModulePath,
        prog: &Program,
    ) -> CompileResult<()> {
        for imp in &prog.impls {
            let Some(trait_name) = constraint_trait_name(&imp.trait_type) else {
                continue;
            };
            // An impl over a generic `for_type` (still mentioning an unbound type, e.g.
            // `implement Foo for WrapperType[T]`) is deferred: we cannot key it to a concrete type
            // yet, so stash it for realization per monomorph. Note `for_type` parameters are parsed
            // as `Named("T")` references, not `TypeParam`, so we detect them as type names that are
            // not yet known concrete templates.
            if for_type_is_templated(&self.monomorphs.templates, &imp.for_type) {
                self.monomorphs.template_impls.push(TemplateImpl {
                    trait_name: trait_name.clone(),
                    for_type: imp.for_type.clone(),
                    methods: imp.methods.clone(),
                    module_path: module_path.clone(),
                });
                continue;
            }
            let for_type = canonical_type(&imp.for_type);
            self.monomorphs
                .impls
                .entry(canonical_type_key(&for_type))
                .or_default()
                .push(ImplEntry {
                    trait_name: trait_name.clone(),
                    for_type: imp.for_type.clone(),
                    methods: imp.methods.clone(),
                });

            for method in &imp.methods {
                let (_, prepared) =
                    self.build_prepared_impl_method(&trait_name, &for_type, method, module_path)?;
                self.monomorphs.impl_methods.push(prepared);
            }
        }
        Ok(())
    }

    /// Build a *prepared* impl method: stable mangled symbol, synthesized `this` receiver
    /// prepended, signature pre-registered in the symbol table and associated with `module_path`
    /// via `mono_modules`. Returns the `(symbol, prepared_function)`; the caller decides where the
    /// prepared function is staged (`impl_methods` for concrete impls, the merged program for
    /// realized template impls). `for_type` must already be the concrete, canonical receiver type.
    pub(crate) fn build_prepared_impl_method(
        &mut self,
        trait_name: &str,
        for_type: &Type,
        method: &Function,
        module_path: &ModulePath,
    ) -> CompileResult<(String, Function)> {
        let symbol = impl_method_symbol(trait_name, for_type, &method.name);
        let mut prepared = method.clone();
        prepared.name = symbol.clone();
        // Prepend the synthesized `this` receiver parameter.
        let mut params = Vec::with_capacity(method.params.len() + 1);
        params.push(Param {
            name: "this".to_string(),
            annotation: Some(for_type.clone()),
            ty: self.store.new_unknown(),
        });
        params.extend(method.params.iter().cloned());
        prepared.params = params;
        // Associate the synthesized method with its declaring module so per-module codegen (which
        // consults `mono_modules`) emits it in the right C file, and so the call site mangles it
        // to the same name.
        self.monomorphs
            .mono_modules
            .insert(symbol.clone(), module_path.clone());

        // Pre-register the signature so call sites resolve before the body is inferred.
        let param_ids: Vec<TypeId> = prepared
            .params
            .iter()
            .map(|p| self.type_id_from_annotation(p.annotation.as_ref(), &HashMap::new()))
            .collect::<Result<_, _>>()?;
        let ret_id =
            self.type_id_from_annotation(prepared.return_type.as_ref(), &HashMap::new())?;
        self.symbols_mut()
            .define_function(&symbol, param_ids, ret_id);

        Ok((symbol, prepared))
    }

    /// Look up the impl of `trait_name` for `for_type`, if one is registered.
    ///
    /// Both constraint checking and method dispatch resolve through this single entry point so the
    /// two features stay consistent.
    pub(crate) fn lookup_impl(&self, trait_name: &str, for_type: &Type) -> Option<&ImplEntry> {
        let key = canonical_type_key(for_type);
        self.monomorphs
            .impls
            .get(&key)
            .and_then(|entries| entries.iter().find(|e| e.trait_name == trait_name))
    }

    /// Resolve a method call `receiver.method()` to the mangled impl-method symbol, or `None` if
    /// the receiver's type has no impl providing that method.
    ///
    /// This is the dispatch half of the impl registry: it reuses `MonomorphState::impls` (the same
    /// table constraint checking resolves through) and matches by method name, not trait name.
    pub(crate) fn lookup_method(&self, receiver_type: &Type, method_name: &str) -> Option<String> {
        let key = canonical_type_key(receiver_type);
        self.monomorphs
            .impls
            .get(&key)?
            .iter()
            .find(|e| e.methods.iter().any(|m| m.name == method_name))
            .map(|e| impl_method_symbol(&e.trait_name, &e.for_type, method_name))
    }

    /// Validate a single impl's methods against its trait declaration: trait exists,
    /// completeness, and signature agreement. Shared by the concrete-impl and template-impl
    /// passes of `validate_trait_impls`.
    fn validate_impl_methods(
        trait_name: &str,
        for_type: &Type,
        methods: &[Function],
        trait_defs: &HashMap<&str, &TraitDefinition>,
    ) -> CompileResult<()> {
        // E. trait exists.
        let Some(trait_def) = trait_defs.get(trait_name) else {
            return Err(type_err!("trait '{}' is not defined", trait_name));
        };
        let declared: HashMap<&str, &Function> = trait_def
            .methods
            .iter()
            .map(|m| (m.name.as_str(), m))
            .collect();
        let provided: HashMap<&str, &Function> =
            methods.iter().map(|m| (m.name.as_str(), m)).collect();
        // B. completeness: every declared method must be provided, and no extras.
        for decl in declared.values() {
            if !provided.contains_key(decl.name.as_str()) {
                return Err(type_err!(
                    "trait '{}' requires method '{}', which is missing from the impl for type '{}'",
                    trait_name,
                    decl.name,
                    for_type.to_c_repr().name
                ));
            }
        }
        for prov in provided.values() {
            let Some(decl) = declared.get(prov.name.as_str()) else {
                return Err(type_err!(
                    "impl of trait '{}' for type '{}' provides method '{}', which is not declared by the trait",
                    trait_name,
                    for_type.to_c_repr().name,
                    prov.name
                ));
            };
            // C. signature agreement.
            if !signatures_match(decl, prov) {
                return Err(type_err!(
                    "method '{}' in the impl of trait '{}' for type '{}' does not match the trait declaration's signature",
                    prov.name,
                    trait_name,
                    for_type.to_c_repr().name
                ));
            }
        }
        Ok(())
    }

    /// Validate trait implementations: a hardening pass that enforces the invariants static method
    /// dispatch and constraint checking rely on but the registry alone does not check.
    ///
    /// - **Duplicate impls**: two `implement Trait for Type` for the same `(trait, for_type)` are
    ///   rejected.
    /// - **Trait exists**: an impl targeting an undefined trait is rejected.
    /// - **Completeness**: every method declared by the trait must be provided by the impl, and
    ///   the impl must not provide methods the trait does not declare.
    /// - **Signature agreement**: each provided method's parameter and return types must match the
    ///   trait declaration's signature.
    ///
    /// `trait_defs` are the merged trait definitions from the driver's merged program, so
    /// cross-module trait/impl pairs are checked consistently. The registry (`impls`) is keyed by
    /// the `for_type`'s canonical name, so duplicates appear as two entries with the same
    /// `trait_name` in one bucket.
    pub(crate) fn validate_trait_impls(&self, trait_defs: &[TraitDefinition]) -> CompileResult<()> {
        let trait_defs: HashMap<&str, &TraitDefinition> =
            trait_defs.iter().map(|t| (t.name.as_str(), t)).collect();
        // Concrete impls (keyed by canonical for_type).
        for entries in self.monomorphs.impls.values() {
            // A. duplicate impls (same trait, same canonical for_type bucket).
            for (i, e) in entries.iter().enumerate() {
                if entries[..i]
                    .iter()
                    .any(|other| other.trait_name == e.trait_name)
                {
                    return Err(type_err!(
                        "duplicate implementation of trait '{}' for type '{}'",
                        e.trait_name,
                        e.for_type.to_c_repr().name
                    ));
                }
            }
            for e in entries {
                Self::validate_impl_methods(&e.trait_name, &e.for_type, &e.methods, &trait_defs)?;
            }
        }
        // Template impls (for_type still mentions a type parameter, realized per monomorph).
        for (i, t) in self.monomorphs.template_impls.iter().enumerate() {
            if self.monomorphs.template_impls[..i]
                .iter()
                .any(|other| other.trait_name == t.trait_name && other.for_type == t.for_type)
            {
                return Err(type_err!(
                    "duplicate implementation of trait '{}' for type '{}'",
                    t.trait_name,
                    t.for_type.to_c_repr().name
                ));
            }
            Self::validate_impl_methods(&t.trait_name, &t.for_type, &t.methods, &trait_defs)?;
        }
        Ok(())
    }

    /// Reset the per-pass monomorphization state.
    ///
    /// Instance variables and pending applications are rebuilt each pass; the realized-monomorph
    /// cache and template stash persist across passes.
    pub(crate) fn begin_monomorph_pass(&mut self) {
        self.monomorphs.instance_types.clear();
        self.monomorphs.pending.clear();
        self.monomorphs.unresolved.clear();
    }

    /// Whether `name` declares a generic (template) struct.
    pub(crate) fn is_template_struct(&self, name: &str) -> bool {
        matches!(
            self.monomorphs.templates.get(name).map(|(_, t)| t),
            Some(TemplateDef::Struct(_))
        )
    }

    /// Whether `name` declares a generic (template) enum.
    pub(crate) fn is_template_enum(&self, name: &str) -> bool {
        matches!(
            self.monomorphs.templates.get(name).map(|(_, t)| t),
            Some(TemplateDef::Enum(_))
        )
    }

    /// Whether `name` declares a generic (template) function.
    pub(crate) fn is_template_function(&self, name: &str) -> bool {
        matches!(
            self.monomorphs.templates.get(name).map(|(_, t)| t),
            Some(TemplateDef::Function(_))
        )
    }

    /// Whether `name` is a realized monomorph name.
    pub(crate) fn is_monomorph_name(&self, name: &str) -> bool {
        self.monomorphs.mono_instances.contains_key(name)
    }

    /// The module that defines the monomorph `name`, if any.
    pub(crate) fn monomorph_module(&self, name: &str) -> Option<&ModulePath> {
        self.monomorphs.mono_modules.get(name)
    }

    /// The declared parameter names of a template, if it exists.
    fn template_param_names(&self, base: &str) -> Option<Vec<String>> {
        self.monomorphs
            .templates
            .get(base)
            .map(|(_, t)| t.type_params().iter().map(|p| p.name.clone()).collect())
    }

    /// Resolve a type annotation to a `TypeId`, instantiating any generic application inside it.
    ///
    /// `params` maps in-scope type parameter names to their bindings while instantiating monomorph
    /// clones (whose bodies still mention `TypeParam`s); it is empty at ordinary annotation sites.
    ///
    /// # Errors
    /// Returns `TypeError` when a `TypeParam` is used outside its generic definition, or when an
    /// inner `Instance` application fails to resolve.
    pub(crate) fn type_id_from_annotation(
        &mut self,
        ann: Option<&Type>,
        params: &HashMap<String, TypeId>,
    ) -> CompileResult<TypeId> {
        let Some(t) = ann else {
            return Ok(self.store.new_unknown());
        };
        match t {
            Type::TypeParam(name) => params.get(name).copied().ok_or_else(|| {
                CompilationError::TypeError(format!(
                    "type parameter '{name}' used outside of its generic definition"
                ))
            }),
            Type::Instance { base, args } => {
                let (_, ty) = self.instance_params_and_type(base, args, params)?;
                Ok(ty)
            }
            Type::Named(name) if self.is_template_struct(name) || self.is_template_enum(name) => {
                let (_, ty) = self.instance_params_and_type(name, &[], params)?;
                Ok(ty)
            }
            other => {
                // Substitute the in-scope parameters (if any), then store the the resulting
                // concrete type. Parameters that are still fresh unknowns (mid-inference call
                // sites) fall back to the raw parameter reference; annotations without
                // `TypeParam`s cache nothing anyway.
                let mut as_types = HashMap::new();
                for (name, id) in params {
                    if let Ok(ty) = self.store.resolve(*id) {
                        as_types.insert(name.clone(), ty);
                    }
                }
                Ok(self.store.new_known(substitute_type(other, &as_types)))
            }
        }
    }

    /// Build the type parameters and result type of a generic application.
    ///
    /// Mirrors the reference's `makeGeneric`:
    /// - supplied arguments are instantiated as given;
    /// - missing parameters become fresh unknowns, or their declared `= default` when one exists;
    /// - when every parameter is concrete the monomorph is realized immediately and the result is
    ///   `Named(monomorph)`;
    /// - otherwise a fresh instance variable is created, the application is enqueued on the pending
    ///   worklist, and `generate_monomorphs` resolves it later.
    ///
    /// # Errors
    /// Returns `TypeError` if `base` is not generic or if more arguments than declared type
    /// parameters are supplied.
    pub(crate) fn instance_params_and_type(
        &mut self,
        base: &str,
        supplied: &[Type],
        params: &HashMap<String, TypeId>,
    ) -> CompileResult<(Vec<TypeId>, TypeId)> {
        let Some((_, def)) = self.monomorphs.templates.get(base) else {
            return Err(type_err!("type '{}' is not generic", base));
        };
        let declared: Vec<TypeParam> = def.type_params().to_vec();
        if supplied.len() > declared.len() {
            return Err(type_err!(
                "type '{}' expects {} type argument(s), got {}",
                base,
                declared.len(),
                supplied.len()
            ));
        }

        let mut ids: Vec<TypeId> = Vec::with_capacity(declared.len());
        for arg in supplied {
            ids.push(self.type_id_from_annotation(Some(arg), params)?);
        }
        for param in &declared[supplied.len()..] {
            if let Some(default) = &param.default {
                ids.push(self.type_id_from_annotation(Some(default), params)?);
            } else {
                // A missing type argument (no supplied value) with no own default: apply default
                // specialization. If a trait constraint on the parameter has a `default Trait as
                // Type` declaration, bind the fresh unknown to that type immediately. This is sound
                // because a missing argument has no other source of information; doing it eagerly
                // also avoids re-creating the variable each fixpoint pass (which would otherwise
                // loop forever on a stuck var).
                let unk = self.store.new_unknown();
                if let Some(default_ty) = crate::codegen::specialize::default_for_constraints(
                    &param.constraints,
                    &self.monomorphs.defaults,
                ) {
                    self.store.bind_if_unbound(unk, default_ty);
                }
                ids.push(unk);
            }
        }

        match ids
            .iter()
            .map(|id| self.store.resolve(*id))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(args) => {
                // Constraint checking: a parameter with trait constraints must bind to a type
                // that implements each one. Checked eagerly whenever the bound argument is already
                // concrete; partially-bound parameters are checked on a later pass once their
                // argument resolves. The same lookup table underpins method dispatch.
                for (param, arg) in declared.iter().zip(args.iter()) {
                    if param.constraints.is_empty() || !is_concrete(arg) {
                        continue;
                    }
                    for constraint in &param.constraints {
                        if let Some(trait_name) = constraint_trait_name(constraint)
                            && self.lookup_impl(&trait_name, arg).is_none()
                        {
                            return Err(type_err!(
                                "type '{}' does not implement trait '{}'",
                                arg.to_c_repr().name,
                                trait_name
                            ));
                        }
                    }
                }
                let name = self.instantiate(base, &args)?.0;
                Ok((ids, self.store.new_known(Type::Named(name))))
            }
            Err(_) => {
                let app = ids.clone();
                let var_id = self.store.new_unknown();
                self.monomorphs.instance_types.insert(
                    var_id,
                    PendingInstance {
                        base: base.to_string(),
                        params: app.clone(),
                    },
                );
                self.monomorphs.pending.push(PendingGeneric {
                    base: base.to_string(),
                    params: app,
                    call_return: None,
                });
                Ok((ids, var_id))
            }
        }
    }

    /// Realize the monomorph `base[args]` (already concretized), or fetch it from cache.
    ///
    /// Clones the template, substitutes its parameters, registers the result in the symbol table,
    /// and stages the declaration so the next pass types and emits it. Returns the monomorph's
    /// name and whether it was newly created.
    ///
    /// # Errors
    /// Returns an internal `TypeError` if the template is missing.
    fn instantiate(
        &mut self,
        base: &str,
        args: &[Type],
    ) -> Result<(String, bool), CompilationError> {
        let key = (base.to_string(), args.to_vec());
        if let Some(name) = self.monomorphs.complete.get(&key) {
            return Ok((name.clone(), false));
        }

        let Some((module, def)) = self.monomorphs.templates.get(base).cloned() else {
            return Err(type_err!("internal error: no template '{}'", base));
        };
        // Impls attach to data types (struct/enum), not generic functions, so only data-type
        // monomorphs can trigger template-impl realization.
        let data_type = matches!(def, TemplateDef::Struct(_) | TemplateDef::Enum(_));
        let mono_name = monomorph_name(base, args);

        let type_map: HashMap<String, Type> = def
            .type_params()
            .iter()
            .zip(args.iter())
            .map(|(p, a)| (p.name.clone(), a.clone()))
            .collect();

        match def {
            TemplateDef::Struct(s) => {
                let mut mono = s;
                mono.name = mono_name.clone();
                // A monomorph is concrete: it is no longer a template.
                mono.type_params.clear();
                for field in &mut mono.fields {
                    field.annotation = field
                        .annotation
                        .as_ref()
                        .map(|a| substitute_type(a, &type_map));
                    field.ty =
                        self.type_id_from_annotation(field.annotation.as_ref(), &HashMap::new())?;
                }
                for field in &mut mono.fields {
                    if let Some(default) = &mut field.default {
                        substitute_expr_annotations(default, &type_map);
                    }
                }
                self.symbols_mut().define_struct(mono.clone());
                self.monomorphs.staged_structs.push(mono);
            }
            TemplateDef::Enum(e) => {
                let mut mono = e;
                mono.name = mono_name.clone();
                // A monomorph is concrete: it is no longer a template.
                mono.type_params.clear();
                for variant in &mut mono.variants {
                    variant.parent = mono_name.clone();
                    for arg in &mut variant.args {
                        arg.annotation = arg
                            .annotation
                            .as_ref()
                            .map(|a| substitute_type(a, &type_map));
                        arg.ty =
                            self.type_id_from_annotation(arg.annotation.as_ref(), &HashMap::new())?;
                    }
                    if let Some(default) = &mut variant.default {
                        substitute_expr_annotations(default, &type_map);
                    }
                }
                self.symbols_mut().define_enum(mono.clone());
                for variant in &mono.variants {
                    self.symbols_mut().define_enum_variant(variant);
                }
                self.monomorphs.staged_enums.push(mono);
            }
            TemplateDef::Function(f) => {
                let mut mono = f;
                mono.name = mono_name.clone();
                // A monomorph is concrete: it is no longer a template.
                mono.type_params.clear();
                for param in &mut mono.params {
                    param.annotation = param
                        .annotation
                        .as_ref()
                        .map(|a| substitute_type(a, &type_map));
                    param.ty =
                        self.type_id_from_annotation(param.annotation.as_ref(), &HashMap::new())?;
                }
                mono.return_type = mono
                    .return_type
                    .as_ref()
                    .map(|r| substitute_type(r, &type_map));
                mono.inferred_return = mono
                    .return_type
                    .as_ref()
                    .map(|r| self.type_id_from_annotation(Some(r), &HashMap::new()))
                    .transpose()?;
                for stmt in &mut mono.body.stmts {
                    substitute_stmt_annotations(stmt, &type_map);
                }
                let param_ids: Vec<TypeId> = mono.params.iter().map(|p| p.ty).collect();
                if let Some(ret_id) = mono.inferred_return {
                    self.symbols_mut()
                        .define_function(&mono.name, param_ids, ret_id);
                }
                self.monomorphs.staged_functions.push(mono);
            }
        }

        self.monomorphs.complete.insert(key, mono_name.clone());
        self.monomorphs
            .mono_modules
            .insert(mono_name.clone(), module);
        self.monomorphs
            .mono_instances
            .insert(mono_name.clone(), (base.to_string(), args.to_vec()));
        // Realize any `implement Trait for Base[T]` impls now that `Base<args>` exists. Staged
        // methods are drained into the merged program by `generate_monomorphs`.
        if data_type {
            self.realize_template_impls(base, args)?;
        }
        Ok((mono_name, true))
    }

    /// Realize any template impls (`implement Foo for WrapperType[T]`) for the generic data type
    /// whose monomorph was just realized as `Instance { base, args }`.
    ///
    /// For each matching template impl we substitute the type parameter(s) into concrete types,
    /// build a concrete `ImplEntry` keyed by the realized type's canonical name (so dispatch finds
    /// it for a `WrapperType[Int]` receiver), and stage the prepared impl methods into
    /// `staged_impl_methods` for `generate_monomorphs` to drain into the merged program. Without
    /// this, such impls would stay registered under an unmatchable template key. Called from
    /// `instantiate` so it fires for both eagerly- and deferred-realized monomorphs.
    /// Idempotent per `(trait, concrete type)`.
    pub(crate) fn realize_template_impls(
        &mut self,
        base: &str,
        args: &[Type],
    ) -> CompileResult<usize> {
        let Some((_, def)) = self.monomorphs.templates.get(base).cloned() else {
            return Ok(0);
        };
        let type_map: HashMap<String, Type> = def
            .type_params()
            .iter()
            .zip(args.iter())
            .map(|(p, a)| (p.name.clone(), a.clone()))
            .collect();
        let concrete_for_type = canonical_type(&Type::Instance {
            base: base.to_string(),
            args: args.to_vec(),
        });

        // Clone the template impls so the per-impl mutable `self` borrows below do not conflict
        // with a held `self.monomorphs` borrow.
        let templates = self.monomorphs.template_impls.clone();
        let mut realized = 0usize;
        for timpl in &templates {
            let Type::Instance {
                base: timpl_base, ..
            } = &timpl.for_type
            else {
                continue;
            };
            if timpl_base != base {
                continue;
            }
            // Already realized for this concrete type (idempotency guard).
            if self
                .lookup_impl(&timpl.trait_name, &concrete_for_type)
                .is_some()
            {
                continue;
            }
            let key = canonical_type_key(&concrete_for_type);
            self.monomorphs
                .impls
                .entry(key)
                .or_default()
                .push(ImplEntry {
                    trait_name: timpl.trait_name.clone(),
                    for_type: concrete_for_type.clone(),
                    methods: timpl
                        .methods
                        .iter()
                        .map(|m| substitute_function(m, &type_map))
                        .collect(),
                });
            for method in &timpl.methods {
                let concrete_method = substitute_function(method, &type_map);
                let (_, prepared) = self.build_prepared_impl_method(
                    &timpl.trait_name,
                    &concrete_for_type,
                    &concrete_method,
                    &timpl.module_path,
                )?;
                self.monomorphs.staged_impl_methods.push(prepared);
                realized += 1;
            }
        }
        Ok(realized)
    }

    /// Resolve the applications recorded during the current pass.
    ///
    /// Every pending application whose parameters are now concrete realizes its monomorph (or
    /// finds it cached), binds every matching instance variable to the monomorph's name, and
    /// rewrites generic-function call sites. The staged monomorph declarations are appended to
    /// `merged` so the next pass types and emits them.
    ///
    /// Returns the number of newly realized monomorphs.
    ///
    /// # Errors
    /// Propagates errors from monomorph realization (`instantiate`) and from resolving staged
    /// declaration types.
    pub(crate) fn generate_monomorphs(&mut self, merged: &mut Program) -> CompileResult<usize> {
        let pending = std::mem::take(&mut self.monomorphs.pending);
        let mut new_count = 0;
        for entry in pending {
            let args = match entry
                .params
                .iter()
                .map(|id| self.store.resolve(*id))
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(args) => args,
                Err(_) => {
                    // The parameters did not resolve this pass (they may never).
                    // Record the failure for the final validation error.
                    let names = self.template_param_names(&entry.base).unwrap_or_default();
                    let first_unresolved = entry
                        .params
                        .iter()
                        .zip(names.iter())
                        .find_map(|(id, name)| {
                            self.store.resolve(*id).is_err().then(|| name.clone())
                        })
                        .unwrap_or_else(|| "<unknown>".to_string());

                    self.monomorphs
                        .unresolved
                        .push((entry.base, first_unresolved));

                    continue;
                }
            };

            let (mono_name, was_new) = self.instantiate(&entry.base, &args)?;
            if was_new {
                new_count += 1;
            }

            // Bind every instance variable standing for this application.
            let matching: Vec<TypeId> = self
                .monomorphs
                .instance_types
                .iter()
                .filter(|(_, inst)| inst.base == entry.base)
                .filter_map(|(var_id, inst)| {
                    if inst
                        .params
                        .iter()
                        .map(|id| self.store.resolve(*id))
                        .collect::<Result<Vec<_>, _>>()
                        .is_ok_and(|resolved| resolved == args)
                    {
                        Some(*var_id)
                    } else {
                        None
                    }
                })
                .collect();
            for var_id in matching {
                self.store
                    .bind_if_unbound(var_id, Type::Named(mono_name.clone()));
            }

            if let Some(ret_id) = entry.call_return {
                Self::rename_generic_calls(&self.store, merged, &entry.base, ret_id, &mono_name);
            }
        }

        merged.structs.append(&mut self.monomorphs.staged_structs);
        merged.enums.append(&mut self.monomorphs.staged_enums);
        merged
            .functions
            .append(&mut self.monomorphs.staged_functions);
        // Impl methods realized from template impls (`implement Foo for WrapperType[T]`) per
        // concrete monomorph: inferred and emitted on the next pass.
        merged
            .functions
            .append(&mut self.monomorphs.staged_impl_methods);
        Ok(new_count)
    }

    /// Rewrite call sites of the generic function `base` to the monomorph `mono_name`.
    ///
    /// A site matches when its callee is `base` and its (substituted) return type resolves to
    /// `ret_id`, which every application creates fresh, so each call is renamed exactly once, to
    /// the monomorph its own invocation produced.
    fn rename_generic_calls(
        store: &crate::codegen::types::TypeStore,
        merged: &mut Program,
        base: &str,
        ret_id: TypeId,
        mono_name: &str,
    ) {
        let ret_rep = store.find_rep(ret_id);
        // Omit template bodies: their expressions are never typed (default
        // TypeIds), so they can never match a call's substituted return type.
        for func in &mut merged.functions {
            for stmt in &mut func.body.stmts {
                Self::rename_generic_calls_stmt(store, stmt, base, ret_rep, mono_name);
            }
        }
        for global in &mut merged.globals {
            if let Some(init) = &mut global.init {
                Self::rename_generic_calls_expr(store, init, base, ret_rep, mono_name);
            }
        }
    }

    fn rename_generic_calls_stmt(
        store: &crate::codegen::types::TypeStore,
        stmt: &mut Stmt,
        base: &str,
        ret_rep: TypeId,
        mono_name: &str,
    ) {
        match &mut stmt.kind {
            StmtKind::VarDecl { init, .. } => {
                if let Some(init) = init {
                    Self::rename_generic_calls_expr(store, init, base, ret_rep, mono_name);
                }
            }
            StmtKind::Expr(e) | StmtKind::Return(Some(e)) => {
                Self::rename_generic_calls_expr(store, e, base, ret_rep, mono_name);
            }
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                Self::rename_generic_calls_expr(store, cond, base, ret_rep, mono_name);
                for s in &mut then_branch.stmts {
                    Self::rename_generic_calls_stmt(store, s, base, ret_rep, mono_name);
                }
                if let Some(b) = else_branch {
                    for s in &mut b.stmts {
                        Self::rename_generic_calls_stmt(store, s, base, ret_rep, mono_name);
                    }
                }
            }
            StmtKind::While { cond, body } => {
                Self::rename_generic_calls_expr(store, cond, base, ret_rep, mono_name);
                for s in &mut body.stmts {
                    Self::rename_generic_calls_stmt(store, s, base, ret_rep, mono_name);
                }
            }
            StmtKind::For { iter, body, .. } => {
                Self::rename_generic_calls_expr(store, iter, base, ret_rep, mono_name);
                for s in &mut body.stmts {
                    Self::rename_generic_calls_stmt(store, s, base, ret_rep, mono_name);
                }
            }
            StmtKind::Match(m) => {
                Self::rename_generic_calls_expr(store, &mut m.expr, base, ret_rep, mono_name);
                for arm in &mut m.arms {
                    for s in &mut arm.body.stmts {
                        Self::rename_generic_calls_stmt(store, s, base, ret_rep, mono_name);
                    }
                }
            }
            StmtKind::Defer { body } => {
                Self::rename_generic_calls_stmt(store, body, base, ret_rep, mono_name);
            }
            StmtKind::Block(block) => {
                for s in &mut block.stmts {
                    Self::rename_generic_calls_stmt(store, s, base, ret_rep, mono_name);
                }
            }
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
    }

    fn rename_generic_calls_expr(
        store: &crate::codegen::types::TypeStore,
        expr: &mut Expr,
        base: &str,
        ret_rep: TypeId,
        mono_name: &str,
    ) {
        if let ExprKind::Call { callee, .. } = &mut expr.kind
            && let ExprKind::Identifier { name } = &mut callee.kind
            && name == base
            && store.find_rep(expr.ty) == ret_rep
        {
            *name = mono_name.to_string();
        }
        match &mut expr.kind {
            ExprKind::Call { callee, args } => {
                Self::rename_generic_calls_expr(store, callee, base, ret_rep, mono_name);
                for a in args {
                    Self::rename_generic_calls_expr(store, a, base, ret_rep, mono_name);
                }
            }
            ExprKind::UnaryOp { expr, .. } => {
                Self::rename_generic_calls_expr(store, expr, base, ret_rep, mono_name);
            }
            ExprKind::BinaryOp { left, right, .. } => {
                Self::rename_generic_calls_expr(store, left, base, ret_rep, mono_name);
                Self::rename_generic_calls_expr(store, right, base, ret_rep, mono_name);
            }
            ExprKind::Assign { left, right, .. } => {
                Self::rename_generic_calls_expr(store, left, base, ret_rep, mono_name);
                Self::rename_generic_calls_expr(store, right, base, ret_rep, mono_name);
            }
            ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                Self::rename_generic_calls_expr(store, cond, base, ret_rep, mono_name);
                Self::rename_generic_calls_expr(store, then_branch, base, ret_rep, mono_name);
                Self::rename_generic_calls_expr(store, else_branch, base, ret_rep, mono_name);
            }
            ExprKind::RangeLiteral { start, end } => {
                Self::rename_generic_calls_expr(store, start, base, ret_rep, mono_name);
                Self::rename_generic_calls_expr(store, end, base, ret_rep, mono_name);
            }
            ExprKind::StructInit { fields, .. } => {
                for f in fields {
                    Self::rename_generic_calls_expr(store, &mut f.value, base, ret_rep, mono_name);
                }
            }
            ExprKind::FieldAccess { expr, .. } => {
                Self::rename_generic_calls_expr(store, expr, base, ret_rep, mono_name);
            }
            ExprKind::Index { expr, index } => {
                Self::rename_generic_calls_expr(store, expr, base, ret_rep, mono_name);
                Self::rename_generic_calls_expr(store, index, base, ret_rep, mono_name);
            }
            ExprKind::EnumInit { args, .. } => {
                for a in args {
                    Self::rename_generic_calls_expr(store, a, base, ret_rep, mono_name);
                }
            }
            ExprKind::ArrayLiteral { elements } => {
                for e in elements {
                    Self::rename_generic_calls_expr(store, e, base, ret_rep, mono_name);
                }
            }
            ExprKind::TupleLit { elements } => {
                for e in elements {
                    Self::rename_generic_calls_expr(store, e, base, ret_rep, mono_name);
                }
            }
            ExprKind::Identifier { .. }
            | ExprKind::Literal { .. }
            | ExprKind::EnumVariant { .. } => {}
        }
    }

    /// The name of the first parameter of `base` that `params` does not resolve,
    /// for error messages.
    fn first_unresolved_param(&self, base: &str, param_ids: &[TypeId]) -> String {
        let names = self.template_param_names(base).unwrap_or_default();
        param_ids
            .iter()
            .zip(names.iter())
            .find_map(|(id, name)| self.store.resolve(*id).is_err().then(|| name.clone()))
            .unwrap_or_else(|| "<unknown>".to_string())
    }

    /// Report generic applications whose type parameters could never be determined.
    ///
    /// # Errors
    /// Returns `TypeError` when an instance variable's parameters never resolved, or when a
    /// generic call's parameters could not be determined (suggesting an explicit `F[Type, ...]`
    /// application or a type annotation).
    pub(crate) fn validate_monomorphs(&self) -> CompileResult<()> {
        // Unbound instance variables: applications whose args never resolved.
        for (var_id, inst) in &self.monomorphs.instance_types {
            let rep = self.store.find_rep(*var_id);
            if self.store.is_unknown(rep) {
                let param = self.first_unresolved_param(&inst.base, &inst.params);
                return Err(type_err!(
                    "cannot determine type parameters for '{}': \
                     type parameter '{}' has no concrete value",
                    inst.base,
                    param,
                ));
            }
        }
        // Calls to generic functions whose parameters never resolved.
        if let Some((base, param)) = self.monomorphs.unresolved.first() {
            return Err(type_err!(
                "cannot determine type arguments for '{}': \
                 type parameter '{}' has no concrete value (try an explicit \
                 `F[Type, ...]` application or a type annotation)",
                base,
                param,
            ));
        }
        Ok(())
    }
}
