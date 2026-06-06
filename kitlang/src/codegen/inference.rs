use std::collections::HashSet;

use super::Field;
use super::ast::{Block, Expr, Function, GlobalDecl, Literal, Program, Stmt};
use super::symbols::{EnumVariantInfo, SymbolTable};
use super::type_ast::{EnumDefinition, FieldInit, StructDefinition};
use super::types::{BinaryOperator, Type, TypeId, TypeStore, UnaryOperator};
use crate::error::{CompilationError, CompileResult};
use crate::type_err;

/// Set the `ty` field on any expression that has one (all except RangeLiteral).
fn set_expr_type(expr: &mut Expr, ty: TypeId) -> &mut Expr {
    match expr {
        Expr::Identifier { ty: t, .. }
        | Expr::Literal { ty: t, .. }
        | Expr::Call { ty: t, .. }
        | Expr::UnaryOp { ty: t, .. }
        | Expr::BinaryOp { ty: t, .. }
        | Expr::Assign { ty: t, .. }
        | Expr::If { ty: t, .. }
        | Expr::StructInit { ty: t, .. }
        | Expr::FieldAccess { ty: t, .. }
        | Expr::EnumVariant { ty: t, .. }
        | Expr::EnumInit { ty: t, .. } => *t = ty,
        Expr::RangeLiteral { .. } => {}
        Expr::ArrayLiteral { ty: t, .. } => *t = ty,
        Expr::Index { ty: t, .. } => *t = ty,
    }
    expr
}

/// Type inference engine using Hindley-Milner algorithm.
#[derive(Default)]
pub struct TypeInferencer {
    pub store: TypeStore,
    symbols: SymbolTable,
    current_return_type: Option<TypeId>,
}

impl TypeInferencer {
    /// Create a new type inferencer with an empty type store and symbol table.
    pub fn new() -> Self {
        Self {
            store: TypeStore::new(),
            symbols: SymbolTable::new(),
            current_return_type: None,
        }
    }

    /// Get a reference to the symbol table (for use by code generation)
    pub fn symbols(&self) -> &SymbolTable {
        &self.symbols
    }

    /// Check if a type name refers to a struct
    pub fn is_struct_type(&self, name: &str) -> bool {
        self.symbols.lookup_struct(name).is_some()
    }

    /// Infer types for an entire program
    pub fn infer_program(&mut self, prog: &mut Program) -> CompileResult<()> {
        self.register_enum_types(&prog.enums);
        self.register_struct_types(&prog.structs);
        self.register_typedefs(&prog.typedefs);

        // Infer global variable types first (before functions)
        self.infer_globals(&mut prog.globals)?;

        for func in &mut prog.functions {
            self.infer_function(func)?;
        }
        Ok(())
    }

    /// Infer types for global variable declarations
    fn infer_globals(&mut self, globals: &mut [GlobalDecl]) -> CompileResult<()> {
        for global in globals {
            if let Some(init_expr) = &mut global.init {
                let init_ty = self.infer_expr(init_expr)?;

                global.inferred = if let Some(ann) = &global.annotation {
                    let ann_ty = self.store.new_known(ann.clone());
                    self.unify(ann_ty, init_ty)?;
                    set_expr_type(init_expr, ann_ty);
                    ann_ty
                } else {
                    init_ty
                };

                self.symbols.define_global(&global.name, global.inferred);
            } else if let Some(ann) = &global.annotation {
                // Declaration without initializer -> just use annotation
                global.inferred = self.store.new_known(ann.clone());
                self.symbols.define_global(&global.name, global.inferred);
            } else {
                return Err(type_err!(
                    "Global variable '{}' declared without type annotation or initializer",
                    global.name
                ));
            }
        }
        Ok(())
    }

    /// Register enum types in the type store and symbol table
    fn register_enum_types(&mut self, enums: &[EnumDefinition]) {
        for enum_def in enums {
            self.symbols.define_enum(enum_def.clone());
            for variant in &enum_def.variants {
                let mut resolved_variant = variant.clone();
                for arg in &mut resolved_variant.args {
                    arg.ty = self.store.known_or_unknown(arg.annotation.as_ref());
                }
                self.symbols.define_enum_variant(&resolved_variant);
            }
        }
    }

    /// Register struct types in the type store and symbol table
    fn register_struct_types(&mut self, structs: &[StructDefinition]) {
        for struct_def in structs {
            // Build field type list and update field types
            let mut updated_fields = Vec::new();
            for field in &struct_def.fields {
                let field_type_id = self.store.known_or_unknown(field.annotation.as_ref());
                updated_fields.push(Field {
                    name: field.name.clone(),
                    ty: field_type_id,
                    annotation: field.annotation.clone(),
                    is_const: field.is_const,
                    default: field.default.clone(),
                });
            }

            // Create updated struct definition with resolved field types
            let updated_struct_def = StructDefinition {
                name: struct_def.name.clone(),
                fields: updated_fields,
                is_public: struct_def.is_public,
                metadata: struct_def.metadata.clone(),
            };

            let field_types: Vec<(String, TypeId)> = updated_struct_def
                .fields
                .iter()
                .map(|field| (field.name.clone(), field.ty))
                .collect();

            // Create struct type and register it
            let struct_type = Type::Struct {
                name: updated_struct_def.name.clone(),
                fields: field_types.clone(),
            };

            let _struct_type_id = self.store.new_known(struct_type);

            // Register updated struct in symbol table for field lookups
            self.symbols.define_struct(updated_struct_def);
        }
    }

    /// Register typedef aliases in the type store so they can be resolved during unification.
    fn register_typedefs(&mut self, typedefs: &[super::type_ast::TypeDef]) {
        for td in typedefs {
            self.store
                .register_typedef(td.name.clone(), td.type_def.clone());
        }
    }

    /// Infer types for a function definition
    fn infer_function(&mut self, func: &mut Function) -> CompileResult<()> {
        // Push a scope for function parameters and body
        self.symbols.push_scope();

        // Infer parameter types (fresh unknowns if unannotated)
        for param in &mut func.params {
            param.ty = self.store.known_or_unknown(param.annotation.as_ref());
            self.symbols.define_var(&param.name, param.ty);
        }

        // Infer return type
        func.inferred_return = self.store.known_or_unknown_some(func.return_type.as_ref());

        self.current_return_type = func.inferred_return;

        // Infer function body
        self.infer_block(&mut func.body)?;

        self.current_return_type = None;

        // Pop function scope (discards params and local vars - they're no longer needed
        // after inference since codegen uses the AST's TypeId fields directly)
        self.symbols.pop_scope();

        // Register function signature in symbol table
        if let Some(ret_ty) = func.inferred_return {
            let param_tys: Vec<TypeId> = func.params.iter().map(|p| p.ty).collect();
            self.symbols.define_function(&func.name, param_tys, ret_ty);
        }

        Ok(())
    }

    /// Infer types for a block of statements
    fn infer_block(&mut self, block: &mut Block) -> CompileResult<()> {
        self.symbols.push_scope();
        for stmt in &mut block.stmts {
            self.infer_stmt(stmt)?;
        }
        self.symbols.pop_scope();
        Ok(())
    }

    /// Infer types for a single statement
    fn infer_stmt(&mut self, stmt: &mut Stmt) -> CompileResult<()> {
        match stmt {
            Stmt::VarDecl {
                name,
                annotation,
                inferred,
                init,
            } => {
                if let Some(init_expr) = init {
                    let init_ty = self.infer_expr(init_expr)?;

                    *inferred = if let Some(ann) = annotation {
                        let ann_ty = self.store.new_known(ann.clone());
                        self.unify(ann_ty, init_ty)?;
                        set_expr_type(init_expr, ann_ty);
                        ann_ty
                    } else {
                        init_ty
                    };

                    self.symbols.define_var(name, *inferred);
                } else if let Some(ann) = annotation {
                    // Declaration without initializer -> just use annotation
                    *inferred = self.store.new_known(ann.clone());
                    self.symbols.define_var(name, *inferred);
                } else {
                    return Err(type_err!(
                        "Variable '{name}' declared without type annotation or initializer",
                    ));
                }
            }

            Stmt::Expr(expr) => {
                self.infer_expr(expr)?;
            }

            Stmt::Return(Some(expr)) => {
                let expr_ty = self.infer_expr(expr)?;
                if let Some(ret_ty) = self.current_return_type {
                    self.unify(ret_ty, expr_ty)?;
                    set_expr_type(expr, ret_ty);
                } else {
                    return Err(type_err!("Return statement outside of function"));
                }
            }

            // Void return - check if function expects void
            Stmt::Return(None) => {
                if let Some(ret_ty) = self.current_return_type {
                    let void_ty = self.store.new_known(Type::Void);
                    self.unify(ret_ty, void_ty)?;
                } else {
                    return Err(type_err!("Return statement outside of function"));
                }
            }

            Stmt::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let cond_ty = self.infer_expr(cond)?;
                let bool_ty = self.store.new_known(Type::Bool);
                self.unify(cond_ty, bool_ty)?;

                self.infer_block(then_branch)?;
                if let Some(else_b) = else_branch {
                    self.infer_block(else_b)?;
                }
            }

            Stmt::While { cond, body } => {
                let cond_ty = self.infer_expr(cond)?;
                let bool_ty = self.store.new_known(Type::Bool);
                self.unify(cond_ty, bool_ty)?;

                self.infer_block(body)?;
            }

            Stmt::For { var, iter, body } => {
                let iter_ty = self.infer_expr(iter)?;

                // NOTE: RangeLiteral is typed as Void (see infer_range_literal),
                // so this accepts both integer-count and range-based for-loops.
                // Accept CArray for iterating over arrays (e.g. `for x in arr`).
                let iter_resolved = self
                    .store
                    .resolve(iter_ty)
                    .map_err(CompilationError::TypeError)?;

                let var_ty = match &iter_resolved {
                    // For CArray, the loop variable gets the element type
                    Type::CArray(elem_type, _) => self.store.new_known(*elem_type.clone()),
                    // Int and Void use int as the loop variable (count-based)
                    Type::Int | Type::Void => self.store.new_known(Type::Int),
                    other => {
                        return Err(type_err!(
                            "For loop iterator must be Int, Range, or Array, found {other:?}"
                        ));
                    }
                };
                self.symbols.define_var(var, var_ty);

                self.infer_block(body)?;
            }

            Stmt::Break | Stmt::Continue => {
                // No type inference needed
            }
        }
        Ok(())
    }

    /// Infer types for an expression
    fn infer_expr(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        Ok(match expr {
            Expr::Identifier { .. } => self.infer_identifier(expr)?,
            Expr::Literal { .. } => self.infer_literal(expr)?,
            Expr::Call { .. } if self.is_call_enum_constructor(expr) => {
                self.infer_enum_constructor_call(expr)?
            }
            Expr::Call { .. } => self.infer_function_call(expr)?,
            Expr::UnaryOp { .. } => self.infer_unary_op(expr)?,
            Expr::BinaryOp { .. } => self.infer_binary_op(expr)?,
            Expr::Assign { .. } => self.infer_assign(expr)?,
            Expr::If { .. } => self.infer_if_expr(expr)?,
            Expr::RangeLiteral { .. } => self.infer_range_literal(expr)?,
            Expr::StructInit { .. } => self.infer_struct_init(expr)?,
            Expr::FieldAccess { .. } => self.infer_field_access(expr)?,
            Expr::EnumVariant { .. } => self.infer_enum_variant(expr)?,
            Expr::EnumInit { .. } => self.infer_enum_init(expr)?,
            Expr::ArrayLiteral { .. } => self.infer_array_literal(expr)?,
            Expr::Index { .. } => self.infer_index(expr)?,
        })
    }

    fn is_call_enum_constructor(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Call { callee, .. } => self
                .symbols
                .lookup_enum_variant_by_simple_name(callee)
                .is_some(),
            _ => false,
        }
    }

    fn infer_identifier(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::Identifier { name, ty: ty_id } = expr else {
            unreachable!("infer_identifier called on non-Identifier");
        };
        if let Some(global_ty) = self.symbols.lookup_global(name) {
            *ty_id = global_ty;
            Ok(global_ty)
        } else if let Some(var_ty) = self.symbols.lookup_var(name) {
            *ty_id = var_ty;
            Ok(var_ty)
        } else if let Some(variant_info) = self.symbols.lookup_enum_variant(name) {
            let enum_ty = self
                .store
                .new_known(Type::Named(variant_info.enum_name.clone()));
            *ty_id = enum_ty;
            *expr = Expr::EnumVariant {
                enum_name: variant_info.enum_name.clone(),
                variant_name: variant_info.variant_name.clone(),
                ty: enum_ty,
            };
            Ok(enum_ty)
        } else {
            // NOTE: fallback - enumerates ALL enums to resolve bare variant names (e.g. `Red`)
            // since earlier paths only find qualified names ("Color.Red") or variables/globals.
            let mut found = None;
            for enum_def in self.symbols.get_enums() {
                for variant in &enum_def.variants {
                    if variant.name == *name {
                        found = Some(enum_def.name.clone());
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            if let Some(enum_name) = found {
                let enum_ty = self.store.new_known(Type::Named(enum_name.clone()));
                *ty_id = enum_ty;
                *expr = Expr::EnumVariant {
                    enum_name: enum_name.clone(),
                    variant_name: name.clone(),
                    ty: enum_ty,
                };
                Ok(enum_ty)
            } else {
                Err(type_err!(
                    "Use of undeclared variable or enum variant '{name}'"
                ))
            }
        }
    }

    fn infer_literal(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::Literal {
            value: lit,
            ty: ty_id,
        } = expr
        else {
            unreachable!("infer_literal called on non-Literal");
        };
        let ty = match lit {
            Literal::Int(_) => Type::Int,
            Literal::Float(_) => Type::Float,
            Literal::Char(_) => Type::Char,
            Literal::Bool(_) => Type::Bool,
            Literal::String(_) => Type::CString,
            Literal::Null => Type::Ptr(Box::new(Type::Void)),
        };
        let type_id = self.store.new_known(ty);
        *ty_id = type_id;
        Ok(type_id)
    }

    fn infer_enum_constructor_call(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::Call { callee, args, ty } = expr else {
            unreachable!("infer_enum_constructor_call called on non-Call");
        };
        let variant_info = self
            .symbols
            .lookup_enum_variant_by_simple_name(callee)
            .expect("guard ensures this exists");
        let args_clone = args.clone();
        let enum_def = self.symbols.lookup_enum(&variant_info.enum_name).cloned();
        let mut resolved_args = if let Some(ref ed) = enum_def {
            Self::resolve_default_args(variant_info, ed, &args_clone)?
        } else {
            args_clone
        };

        if resolved_args.len() != variant_info.arg_types.len() {
            return Err(type_err!(
                "Enum variant '{}' expects {} arguments, got {}",
                variant_info.variant_name,
                variant_info.arg_types.len(),
                resolved_args.len()
            ));
        }

        let expected_types: Vec<_> = variant_info.arg_types.to_vec();
        let enum_ty = self
            .store
            .new_known(Type::Named(variant_info.enum_name.clone()));
        for (arg, expected_ty) in resolved_args.iter_mut().zip(expected_types.iter()) {
            let arg_ty = self.infer_expr(arg)?;
            self.unify(arg_ty, *expected_ty)?;
            set_expr_type(arg, *expected_ty);
        }
        *args = resolved_args;
        *ty = enum_ty;
        Ok(enum_ty)
    }

    fn infer_function_call(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::Call { callee, args, ty } = expr else {
            unreachable!("infer_function_call called on non-Call");
        };
        let (param_tys, ret_ty) = if let Some(sig) = self.symbols.lookup_function(callee) {
            sig
        } else {
            let void_ty = self.store.new_known(Type::Void);
            (vec![], void_ty)
        };

        if !param_tys.is_empty() && args.len() != param_tys.len() {
            return Err(type_err!(
                "Function '{}' expects {} arguments, got {}",
                callee,
                param_tys.len(),
                args.len()
            ));
        }

        if param_tys.is_empty() {
            for arg in args.iter_mut() {
                self.infer_expr(arg)?;
            }
        } else {
            for (arg, param_ty) in args.iter_mut().zip(param_tys.iter()) {
                let arg_ty = self.infer_expr(arg)?;
                self.unify(arg_ty, *param_ty)?;
                set_expr_type(arg, *param_ty);
            }
        }

        *ty = ret_ty;
        Ok(ret_ty)
    }

    fn infer_unary_op(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::UnaryOp {
            op,
            expr: inner,
            ty,
        } = expr
        else {
            unreachable!("infer_unary_op called on non-UnaryOp");
        };
        let expr_ty = self.infer_expr(inner)?;

        let result_ty = match op {
            UnaryOperator::AddressOf => {
                let resolved = self
                    .store
                    .resolve(expr_ty)
                    .map_err(CompilationError::TypeError)?;
                let ptr_ty = Type::Ptr(Box::new(resolved));
                self.store.new_known(ptr_ty)
            }
            UnaryOperator::Dereference => {
                let resolved = self
                    .store
                    .resolve(expr_ty)
                    .map_err(CompilationError::TypeError)?;
                if let Type::Ptr(inner_ty) = resolved {
                    self.store.new_known(*inner_ty)
                } else {
                    return Err(type_err!(
                        "Cannot dereference non-pointer type: {resolved:?}"
                    ));
                }
            }
            _ => expr_ty,
        };

        *ty = result_ty;
        Ok(result_ty)
    }

    fn infer_binary_op(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::BinaryOp {
            op,
            left,
            right,
            ty,
        } = expr
        else {
            unreachable!("infer_binary_op called on non-BinaryOp");
        };
        let left_ty = self.infer_expr(left)?;
        let right_ty = self.infer_expr(right)?;

        let result_ty = match op {
            BinaryOperator::And | BinaryOperator::Or => {
                let bool_ty = self.store.new_known(Type::Bool);
                self.unify(left_ty, bool_ty)?;
                self.unify(right_ty, bool_ty)?;
                bool_ty
            }
            BinaryOperator::Eq
            | BinaryOperator::Ne
            | BinaryOperator::Lt
            | BinaryOperator::Gt
            | BinaryOperator::Le
            | BinaryOperator::Ge => {
                self.unify(left_ty, right_ty)?;
                self.store.new_known(Type::Bool)
            }
            _ => {
                self.unify(left_ty, right_ty)?;
                left_ty
            }
        };

        *ty = result_ty;
        Ok(result_ty)
    }

    fn infer_assign(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::Assign {
            op: _,
            left,
            right,
            ty,
        } = expr
        else {
            unreachable!("infer_assign called on non-Assign");
        };
        let right_ty = self.infer_expr(right)?;
        let left_ty = self.infer_expr(left)?;

        self.unify(left_ty, right_ty)?;

        *ty = left_ty;
        Ok(left_ty)
    }

    fn infer_if_expr(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::If {
            cond,
            then_branch,
            else_branch,
            ty,
        } = expr
        else {
            unreachable!("infer_if_expr called on non-If");
        };
        let cond_ty = self.infer_expr(cond)?;
        let bool_ty = self.store.new_known(Type::Bool);
        self.unify(cond_ty, bool_ty)?;

        let then_ty = self.infer_expr(then_branch)?;
        let else_ty = self.infer_expr(else_branch)?;

        self.unify(then_ty, else_ty)?;

        *ty = then_ty;
        Ok(then_ty)
    }

    fn infer_range_literal(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::RangeLiteral { start, end } = expr else {
            unreachable!("infer_range_literal called on non-RangeLiteral");
        };
        let start_ty = self.infer_expr(start)?;
        let end_ty = self.infer_expr(end)?;

        let int_ty = self.store.new_known(Type::Int);
        self.unify(start_ty, int_ty)?;
        self.unify(end_ty, int_ty)?;

        Ok(self.store.new_known(Type::Void))
    }

    fn infer_struct_init(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::StructInit {
            ty,
            struct_type,
            fields,
        } = expr
        else {
            unreachable!("infer_struct_init called on non-StructInit");
        };

        let resolved_ty = if let Some(ref st) = *struct_type {
            self.store.new_known(st.clone())
        } else {
            return Err(type_err!("StructInit missing type annotation"));
        };

        // resolve struct type from annotation
        let struct_def = {
            let resolved = self
                .store
                .resolve(resolved_ty)
                .map_err(CompilationError::TypeError)?;
            match resolved {
                Type::Named(name) => self
                    .symbols
                    .lookup_struct(&name)
                    .ok_or_else(|| type_err!("Unknown struct type '{name}'"))?,
                Type::Struct { name, .. } => self
                    .symbols
                    .lookup_struct(&name)
                    .ok_or_else(|| type_err!("Unknown struct type '{name}'"))?,
                _ => return Err(type_err!("StructInit requires a struct type")),
            }
        };

        // validate provided field names + check required fields
        let provided_field_names: HashSet<String> = fields.iter().map(|f| f.name.clone()).collect();

        for field_init in fields.iter() {
            if !struct_def.fields.iter().any(|f| f.name == field_init.name) {
                return Err(type_err!(
                    "Struct '{}' has no field '{}'",
                    struct_def.name,
                    field_init.name
                ));
            }
        }

        for field_def in &struct_def.fields {
            if !provided_field_names.contains(&field_def.name) && field_def.default.is_none() {
                return Err(type_err!(
                    "Struct '{}' field '{}' has no default value and was not provided in initialization",
                    struct_def.name,
                    field_def.name
                ));
            }
        }

        let field_infos: Vec<(String, Option<Type>, Option<Expr>)> = struct_def
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.annotation.clone(), f.default.clone()))
            .collect();

        let _ = struct_def;

        // inject default values for missing optional fields
        for field_info in &field_infos {
            let field_name = &field_info.0;
            if !provided_field_names.contains(field_name)
                && let Some(default_expr) = &field_info.2
            {
                fields.push(FieldInit {
                    name: field_name.clone(),
                    value: default_expr.clone(),
                });
            }
        }

        // infer and unify each field value against its declared type
        for field_init in fields.iter_mut() {
            let field_info = field_infos
                .iter()
                .find(|fi| fi.0 == field_init.name)
                .ok_or_else(|| {
                    type_err!("Struct field '{}' not found in definition", field_init.name)
                })?;

            let inferred_ty = self.infer_expr(&mut field_init.value)?;

            let expected_ty = if let Some(ref ann) = field_info.1 {
                self.store.new_known(ann.clone())
            } else {
                inferred_ty
            };

            self.unify(inferred_ty, expected_ty)?;
        }

        *ty = resolved_ty;
        Ok(resolved_ty)
    }

    fn infer_field_access(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::FieldAccess {
            expr: inner,
            field_name,
            ty: field_ty,
        } = expr
        else {
            unreachable!("infer_field_access called on non-FieldAccess");
        };

        let container_ty = self.infer_expr(inner)?;

        let resolved = self
            .store
            .resolve(container_ty)
            .map_err(CompilationError::TypeError)?;

        let (struct_name, fields) = match resolved {
            Type::Struct { name, fields } => (name, fields),
            Type::Named(type_name) => {
                if let Some(struct_def) = self.symbols.lookup_struct(&type_name) {
                    let fields: Vec<(String, TypeId)> = struct_def
                        .fields
                        .iter()
                        .map(|f| (f.name.clone(), f.ty))
                        .collect();
                    (type_name, fields)
                } else if let Some(enum_def) = self.symbols.lookup_enum(&type_name) {
                    if let Some(variant) = enum_def
                        .variants
                        .iter()
                        .find(|v| v.args.iter().any(|a| a.name == *field_name))
                    {
                        let fields: Vec<(String, TypeId)> = variant
                            .args
                            .iter()
                            .map(|f| (f.name.clone(), f.ty))
                            .collect();
                        (type_name, fields)
                    } else {
                        return Err(type_err!(
                            "Enum '{}' has no field '{}'",
                            type_name,
                            field_name
                        ));
                    }
                } else {
                    return Err(type_err!(
                        "Cannot access field on unknown type '{}'",
                        type_name
                    ));
                }
            }
            _ => return Err(type_err!("Cannot access field on non-struct type")),
        };

        let field_type_id = fields
            .iter()
            .find(|(fname, _)| fname == field_name)
            .ok_or_else(|| {
                type_err!(
                    "Struct/variant '{}' has no field '{}'",
                    struct_name,
                    field_name
                )
            })?
            .1;

        *field_ty = field_type_id;
        Ok(field_type_id)
    }

    fn infer_enum_variant(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::EnumVariant {
            enum_name,
            variant_name,
            ty,
        } = expr
        else {
            unreachable!("infer_enum_variant called on non-EnumVariant");
        };
        let _variant_info = self
            .symbols
            .lookup_variant(enum_name, variant_name)
            .ok_or_else(|| type_err!("Unknown enum variant '{}.{}'", enum_name, variant_name))?;

        let enum_ty = self.store.new_known(Type::Named(enum_name.clone()));
        *ty = enum_ty;
        Ok(enum_ty)
    }

    fn infer_enum_init(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::EnumInit {
            enum_name,
            variant_name,
            args,
            ty,
        } = expr
        else {
            unreachable!("infer_enum_init called on non-EnumInit");
        };

        let (variant_info, enum_def) = {
            let info = self
                .symbols
                .lookup_variant(enum_name, variant_name)
                .ok_or_else(|| type_err!("Unknown enum variant '{}.{}'", enum_name, variant_name))?
                .clone();

            let enum_def = self
                .symbols
                .lookup_enum(enum_name)
                .ok_or_else(|| type_err!("Unknown enum '{}'", enum_name))?
                .clone();

            (info, enum_def)
        };

        let resolved_args = Self::resolve_default_args(&variant_info, &enum_def, args)?;
        *args = resolved_args;

        if args.len() != variant_info.arg_types.len() {
            return Err(type_err!(
                "Enum variant '{}.{}' expects {} arguments, got {}",
                enum_name,
                variant_name,
                variant_info.arg_types.len(),
                args.len()
            ));
        }

        for (arg, &expected_ty) in args.iter_mut().zip(variant_info.arg_types.iter()) {
            let arg_ty = self.infer_expr(arg)?;
            self.unify(arg_ty, expected_ty)?;
            set_expr_type(arg, expected_ty);
        }

        let enum_ty = self.store.new_known(Type::Named(enum_name.clone()));
        *ty = enum_ty;
        Ok(enum_ty)
    }

    /// Infer type for an array literal expression.
    /// All elements must unify to the same type, and the result is `CArray(element_type, len)`.
    /// Empty array literals (`[]`) are rejected because the element type can't be determined.
    fn infer_array_literal(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::ArrayLiteral { elements, ty } = expr else {
            unreachable!("infer_array_literal called on non-ArrayLiteral");
        };

        // At least one element needed to infer the element type
        if elements.is_empty() {
            return Err(type_err!(
                "Empty array literal '[]' is not supported; add at least one element or a type annotation"
            ));
        }

        // Infer the first element's type as the element type
        let elem_ty_id = self.infer_expr(&mut elements[0])?;

        // Unify all remaining elements with the first element's type
        for elem in elements.iter_mut().skip(1) {
            let e_ty = self.infer_expr(elem)?;
            self.unify(elem_ty_id, e_ty)?;
            set_expr_type(elem, elem_ty_id);
        }

        // Resolve the element type to store it concretely in the CArray type
        let elem_ty = self
            .store
            .resolve(elem_ty_id)
            .map_err(|e| type_err!("Failed to resolve array element type: {e}"))?;
        let array_ty = Type::CArray(Box::new(elem_ty), elements.len());
        *ty = self.store.new_known(array_ty);
        Ok(*ty)
    }

    /// Infer type for an array index expression (e.g., `arr[i]`).
    /// Resolves the container to get the element type, and unifies the index with Int.
    fn infer_index(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let Expr::Index {
            expr: container,
            index,
            ty,
        } = expr
        else {
            unreachable!("infer_index called on non-Index");
        };
        let container_ty = self.infer_expr(container)?;
        let index_ty = self.infer_expr(index)?;

        let int_ty = self.store.new_known(Type::Int);
        self.unify(index_ty, int_ty)?;

        let resolved = self
            .store
            .resolve(container_ty)
            .map_err(CompilationError::TypeError)?;
        let elem_ty = match resolved {
            Type::CArray(elem_type, _) => self.store.new_known(*elem_type),
            Type::Ptr(inner) => self.store.new_known(*inner),
            _ => {
                return Err(type_err!("Cannot index non-array type: {resolved:?}"));
            }
        };
        *ty = elem_ty;
        Ok(elem_ty)
    }

    /// Resolve default arguments for enum variant constructors.
    /// Returns a new Vec with default values filled in.
    /// Follows the Haskell compiler's `addDefaultArgs` function.
    fn resolve_default_args(
        variant_info: &EnumVariantInfo,
        enum_def: &EnumDefinition,
        provided_args: &[Expr],
    ) -> CompileResult<Vec<Expr>> {
        let total_required = variant_info.arg_types.len();
        let mut result = provided_args.to_vec();

        if result.len() < total_required {
            let variant = enum_def
                .variants
                .iter()
                .find(|v| v.name == variant_info.variant_name)
                .ok_or_else(|| {
                    type_err!(
                        "Variant '{}' not found in enum '{}'",
                        variant_info.variant_name,
                        variant_info.enum_name
                    )
                })?;

            let provided_len = result.len();
            for i in provided_len..total_required {
                if let Some(default_expr) = variant.args.get(i).and_then(|f| f.default.as_ref()) {
                    result.push(default_expr.clone());
                }
            }
        }
        Ok(result)
    }

    /// Unify two type IDs
    fn unify(&mut self, a: TypeId, b: TypeId) -> CompileResult<()> {
        self.store.unify(a, b).map_err(CompilationError::TypeError)
    }
}
