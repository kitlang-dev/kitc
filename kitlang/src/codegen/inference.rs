use std::collections::HashSet;

use super::Field;
use super::ast::{Block, Expr, Function, GlobalDecl, Literal, Program, Stmt};
use super::symbols::{EnumVariantInfo, SymbolTable};
use super::type_ast::{EnumDefinition, FieldInit, StructDefinition};
use super::types::{BinaryOperator, Type, TypeId, TypeStore, UnaryOperator};
use crate::error::{CompilationError, CompileResult};

/// Type inference engine using Hindley-Milner algorithm.
pub struct TypeInferencer {
    pub store: TypeStore,
    symbols: SymbolTable,
    current_return_type: Option<TypeId>,
}

impl Default for TypeInferencer {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeInferencer {
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
        self.register_enum_types(&prog.enums)?;
        self.register_struct_types(&prog.structs)?;

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
                return Err(CompilationError::TypeError(format!(
                    "Global variable '{}' declared without type annotation or initializer",
                    global.name
                )));
            }
        }
        Ok(())
    }

    /// Register enum types in the type store and symbol table
    fn register_enum_types(&mut self, enums: &[EnumDefinition]) -> CompileResult<()> {
        for enum_def in enums {
            self.symbols.define_enum(enum_def.clone());
            for variant in &enum_def.variants {
                self.symbols.define_enum_variant(variant);
            }
        }
        Ok(())
    }

    /// Register struct types in the type store and symbol table
    fn register_struct_types(&mut self, structs: &[StructDefinition]) -> CompileResult<()> {
        for struct_def in structs {
            // Build field type list and update field types
            let mut updated_fields = Vec::new();
            for field in &struct_def.fields {
                let field_type_id = if let Some(ann) = &field.annotation {
                    self.store.new_known(ann.clone())
                } else {
                    self.store.new_unknown()
                };
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
        Ok(())
    }

    /// Infer types for a function definition
    fn infer_function(&mut self, func: &mut Function) -> CompileResult<()> {
        // Infer parameter types (fresh unknowns if unannotated)
        for param in &mut func.params {
            param.ty = if let Some(ann) = &param.annotation {
                self.store.new_known(ann.clone())
            } else {
                self.store.new_unknown()
            };
            self.symbols.define_var(&param.name, param.ty);
        }

        // Infer return type
        func.inferred_return = if let Some(ann) = &func.return_type {
            Some(self.store.new_known(ann.clone()))
        } else {
            Some(self.store.new_unknown())
        };

        self.current_return_type = func.inferred_return;

        // Infer function body
        self.infer_block(&mut func.body)?;

        self.current_return_type = None;

        // Register function signature in symbol table
        if let Some(ret_ty) = func.inferred_return {
            let param_tys: Vec<TypeId> = func.params.iter().map(|p| p.ty).collect();
            self.symbols.define_function(&func.name, param_tys, ret_ty);
        }

        Ok(())
    }

    /// Infer types for a block of statements
    fn infer_block(&mut self, block: &mut Block) -> CompileResult<()> {
        for stmt in &mut block.stmts {
            self.infer_stmt(stmt)?;
        }
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
                    return Err(CompilationError::TypeError(format!(
                        "Variable '{name}' declared without type annotation or initializer",
                    )));
                }
            }

            Stmt::Expr(expr) => {
                self.infer_expr(expr)?;
            }

            Stmt::Return(Some(expr)) => {
                let expr_ty = self.infer_expr(expr)?;
                if let Some(ret_ty) = self.current_return_type {
                    self.unify(ret_ty, expr_ty)?;
                } else {
                    return Err(CompilationError::TypeError(
                        "Return statement outside of function".into(),
                    ));
                }
            }

            // Void return - check if function expects void
            Stmt::Return(None) => {
                if let Some(ret_ty) = self.current_return_type {
                    let void_ty = self.store.new_known(Type::Void);
                    self.unify(ret_ty, void_ty)?;
                } else {
                    return Err(CompilationError::TypeError(
                        "Return statement outside of function".into(),
                    ));
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

                // For i in N: iter should be Int-like OR a range (which we currently typed as Void)
                // TODO: Better range typing
                let iter_resolved = self
                    .store
                    .resolve(iter_ty)
                    .map_err(CompilationError::TypeError)?;
                if iter_resolved != Type::Int && iter_resolved != Type::Void {
                    return Err(CompilationError::TypeError(format!(
                        "For loop iterator must be Int or Range, found {iter_resolved:?}"
                    )));
                }

                let var_ty = self.store.new_known(Type::Int);
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
        let ty = match expr {
            Expr::Identifier(name, ty_id) => {
                // First check if it's a global variable
                if let Some(global_ty) = self.symbols.lookup_global(name) {
                    *ty_id = global_ty;
                    global_ty
                } else if let Some(var_ty) = self.symbols.lookup_var(name) {
                    *ty_id = var_ty;
                    var_ty
                } else {
                    // Check if this is an enum variant reference (simple variant)
                    // First try qualified name lookup
                    if let Some(variant_info) = self.symbols.lookup_enum_variant(name) {
                        let enum_ty = self
                            .store
                            .new_known(Type::Named(variant_info.enum_name.clone()));
                        *ty_id = enum_ty;

                        // Transform to EnumVariant expression for proper code generation
                        *expr = Expr::EnumVariant {
                            enum_name: variant_info.enum_name.clone(),
                            variant_name: variant_info.variant_name.clone(),
                            ty: enum_ty,
                        };

                        enum_ty
                    } else {
                        // Try to find variant by simple name across all enums
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

                            // Transform to EnumVariant expression for proper code generation
                            *expr = Expr::EnumVariant {
                                enum_name: enum_name.clone(),
                                variant_name: name.clone(),
                                ty: enum_ty,
                            };

                            enum_ty
                        } else {
                            return Err(CompilationError::TypeError(format!(
                                "Use of undeclared variable or enum variant '{name}'"
                            )));
                        }
                    }
                }
            }

            Expr::Literal(lit, ty_id) => {
                let ty = match lit {
                    Literal::Int(_) => Type::Int,
                    Literal::Float(_) => Type::Float,
                    Literal::Bool(_) => Type::Bool,
                    Literal::String(_) => Type::CString,
                    Literal::Null => Type::Ptr(Box::new(Type::Void)),
                };
                let type_id = self.store.new_known(ty);
                *ty_id = type_id;
                type_id
            }

            Expr::Call { callee, args, ty } => {
                // Check if this is actually an enum variant constructor call
                if let Some(variant_info) = self.symbols.lookup_enum_variant_by_simple_name(callee)
                {
                    // Clone args before transformation
                    let args_clone = args.clone();

                    // This is an enum variant constructor with arguments
                    let enum_def = self.symbols.lookup_enum(&variant_info.enum_name).cloned();

                    // Resolve default arguments
                    let mut resolved_args = if let Some(ref ed) = enum_def {
                        self.resolve_default_args(variant_info, ed, &args_clone)?
                    } else {
                        args_clone
                    };

                    // Update the args in the expression with resolved defaults
                    *args = resolved_args.clone();

                    let enum_ty = self
                        .store
                        .new_known(Type::Named(variant_info.enum_name.clone()));

                    // Infer types for the resolved arguments
                    for arg in resolved_args.iter_mut() {
                        self.infer_expr(arg)?;
                    }

                    *ty = enum_ty;
                    enum_ty
                } else {
                    let (param_tys, ret_ty) =
                        if let Some(sig) = self.symbols.lookup_function(callee) {
                            sig
                        } else {
                            // For undeclared functions (like printf), we allow them but can't check params.
                            // We assume they return Void for now, or we could return a fresh unknown.
                            let void_ty = self.store.new_known(Type::Void);
                            (vec![], void_ty)
                        };

                    if !param_tys.is_empty() && args.len() != param_tys.len() {
                        return Err(CompilationError::TypeError(format!(
                            "Function '{}' expects {} arguments, got {}",
                            callee,
                            param_tys.len(),
                            args.len()
                        )));
                    }

                    if param_tys.is_empty() {
                        // Just infer arguments without unifying if signature is unknown (variadic C funcs)
                        for arg in args.iter_mut() {
                            self.infer_expr(arg)?;
                        }
                    } else {
                        for (arg, param_ty) in args.iter_mut().zip(param_tys.iter()) {
                            let arg_ty = self.infer_expr(arg)?;
                            self.unify(arg_ty, *param_ty)?;
                        }
                    }

                    *ty = ret_ty;
                    ret_ty
                }
            }

            Expr::UnaryOp { op, expr, ty } => {
                let expr_ty = self.infer_expr(expr)?;

                // Unary operators typically preserve type (except address-of)
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
                            return Err(CompilationError::TypeError(format!(
                                "Cannot dereference non-pointer type: {resolved:?}"
                            )));
                        }
                    }
                    _ => expr_ty,
                };

                *ty = result_ty;
                result_ty
            }

            Expr::BinaryOp {
                op,
                left,
                right,
                ty,
            } => {
                let left_ty = self.infer_expr(left)?;
                let right_ty = self.infer_expr(right)?;

                // Result type depends on operator
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
                result_ty
            }

            Expr::Assign {
                op: _,
                left,
                right,
                ty,
            } => {
                let right_ty = self.infer_expr(right)?;
                let left_ty = self.infer_expr(left)?;

                // Assignment: right must unify with left
                self.unify(left_ty, right_ty)?;

                *ty = left_ty;
                left_ty
            }

            Expr::If {
                cond,
                then_branch,
                else_branch,
                ty,
            } => {
                let cond_ty = self.infer_expr(cond)?;
                let bool_ty = self.store.new_known(Type::Bool);
                self.unify(cond_ty, bool_ty)?;

                let then_ty = self.infer_expr(then_branch)?;
                let else_ty = self.infer_expr(else_branch)?;

                self.unify(then_ty, else_ty)?;

                *ty = then_ty;
                then_ty
            }

            Expr::RangeLiteral { start, end } => {
                // Range literals are only used in for loop context
                let start_ty = self.infer_expr(start)?;
                let end_ty = self.infer_expr(end)?;

                let int_ty = self.store.new_known(Type::Int);
                self.unify(start_ty, int_ty)?;
                self.unify(end_ty, int_ty)?;

                // Return a dummy type -> ranges don't have their own type
                self.store.new_known(Type::Void)
            }

            Expr::StructInit {
                ty,
                struct_type,
                fields,
            } => {
                // Resolve the struct type from the annotation
                let resolved_ty = if let Some(ref st) = *struct_type {
                    self.store.new_known(st.clone())
                } else {
                    return Err(CompilationError::TypeError(
                        "StructInit missing type annotation".into(),
                    ));
                };

                // Look up struct definition in symbol table using the resolved type
                let struct_def = {
                    let resolved = self.store.resolve(resolved_ty)?;
                    match resolved {
                        Type::Named(name) => {
                            self.symbols.lookup_struct(&name).ok_or_else(|| {
                                CompilationError::TypeError(format!("Unknown struct type '{name}'"))
                            })?
                        }
                        Type::Struct { name, .. } => {
                            self.symbols.lookup_struct(&name).ok_or_else(|| {
                                CompilationError::TypeError(format!("Unknown struct type '{name}'"))
                            })?
                        }
                        _ => {
                            return Err(CompilationError::TypeError(
                                "StructInit requires a struct type".into(),
                            ));
                        }
                    }
                };

                // Build set of provided field names for validation
                let provided_field_names: HashSet<String> =
                    fields.iter().map(|f| f.name.clone()).collect();

                // Validate all provided fields exist in struct
                for field_init in fields.iter() {
                    if !struct_def.fields.iter().any(|f| f.name == field_init.name) {
                        return Err(CompilationError::TypeError(format!(
                            "Struct '{}' has no field '{}'",
                            struct_def.name, field_init.name
                        )));
                    }
                }

                // Validate all required fields are provided or have defaults
                for field_def in &struct_def.fields {
                    if !provided_field_names.contains(&field_def.name)
                        && field_def.default.is_none()
                    {
                        return Err(CompilationError::TypeError(format!(
                            "Struct '{}' field '{}' has no default value and was not provided in initialization",
                            struct_def.name, field_def.name
                        )));
                    }
                }

                // Collect field info we need (release struct_def borrow afterwards)
                let field_infos: Vec<(String, Option<Type>, Option<Expr>)> = struct_def
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), f.annotation.clone(), f.default.clone()))
                    .collect();

                // Release the borrow on self.symbols
                let _ = struct_def;

                // Inject default values for missing fields
                for field_info in &field_infos {
                    let field_name = &field_info.0;
                    if !provided_field_names.contains(field_name)
                        && let Some(default_expr) = &field_info.2
                    {
                        // Clone the default expression and add it to fields
                        fields.push(FieldInit {
                            name: field_name.clone(),
                            value: default_expr.clone(),
                        });
                    }
                }

                // Infer types for all field initializer expressions (including defaults)
                for field_init in fields.iter_mut() {
                    // Find the corresponding field info
                    let field_info = field_infos
                        .iter()
                        .find(|fi| fi.0 == field_init.name)
                        .expect("field should exist");

                    // Infer the type of the initializer expression directly (not cloned)
                    let inferred_ty = self.infer_expr(&mut field_init.value)?;

                    // Get the expected field type
                    let expected_ty = if let Some(ref ann) = field_info.1 {
                        self.store.new_known(ann.clone())
                    } else {
                        inferred_ty
                    };

                    // Unify
                    self.unify(inferred_ty, expected_ty)?;
                }

                *ty = resolved_ty;
                resolved_ty
            }

            Expr::FieldAccess {
                expr,
                field_name,
                ty: field_ty,
            } => {
                let container_ty = self.infer_expr(expr)?;

                // Resolve container type, handle both Struct and Named types
                let resolved = self.store.resolve(container_ty)?;

                // For Named types, we need to look up the struct or enum definition
                let (struct_name, fields) = match resolved {
                    Type::Struct { name, fields } => (name, fields),
                    Type::Named(type_name) => {
                        // First try to look up as struct
                        if let Some(struct_def) = self.symbols.lookup_struct(&type_name) {
                            let fields: Vec<(String, TypeId)> = struct_def
                                .fields
                                .iter()
                                .map(|f| (f.name.clone(), f.ty))
                                .collect();
                            (type_name, fields)
                        } else if let Some(enum_def) = self.symbols.lookup_enum(&type_name) {
                            // For enum field access like `d1.VariantName.field`,
                            // we need to check if the field_name is actually a variant name
                            if let Some(variant) =
                                enum_def.variants.iter().find(|v| v.name == *field_name)
                            {
                                // The field access is on the variant's fields
                                // Return the variant's args as fields
                                let fields: Vec<(String, TypeId)> = variant
                                    .args
                                    .iter()
                                    .map(|f| (f.name.clone(), f.ty))
                                    .collect();
                                (type_name, fields)
                            } else {
                                return Err(CompilationError::TypeError(format!(
                                    "Enum '{}' has no variant '{}'",
                                    type_name, field_name
                                )));
                            }
                        } else {
                            return Err(CompilationError::TypeError(format!(
                                "Cannot access field on unknown type '{}'",
                                type_name
                            )));
                        }
                    }
                    _ => {
                        return Err(CompilationError::TypeError(
                            "Cannot access field on non-struct type".into(),
                        ));
                    }
                };

                // Look up field in struct/variant
                let field_type_id = fields
                    .iter()
                    .find(|(fname, _)| fname == field_name)
                    .ok_or_else(|| {
                        CompilationError::TypeError(format!(
                            "Struct/variant '{}' has no field '{}'",
                            struct_name, field_name
                        ))
                    })?
                    .1;

                *field_ty = *field_type_id;
                *field_type_id
            }

            Expr::EnumVariant {
                enum_name,
                variant_name,
                ty,
            } => {
                let _variant_info = self
                    .symbols
                    .lookup_variant(enum_name, variant_name)
                    .ok_or_else(|| {
                        CompilationError::TypeError(format!(
                            "Unknown enum variant '{}.{}'",
                            enum_name, variant_name
                        ))
                    })?;

                // Create a named type for the enum
                let enum_ty = self.store.new_known(Type::Named(enum_name.clone()));
                *ty = enum_ty;
                enum_ty
            }

            Expr::EnumInit {
                enum_name,
                variant_name,
                args,
                ty,
            } => {
                let (variant_info, enum_def) = {
                    let info = self
                        .symbols
                        .lookup_variant(enum_name, variant_name)
                        .ok_or_else(|| {
                            CompilationError::TypeError(format!(
                                "Unknown enum variant '{}.{}'",
                                enum_name, variant_name
                            ))
                        })?
                        .clone();

                    let enum_def = self
                        .symbols
                        .lookup_enum(enum_name)
                        .ok_or_else(|| {
                            CompilationError::TypeError(format!("Unknown enum '{}'", enum_name))
                        })?
                        .clone();

                    (info, enum_def)
                };

                // Resolve default arguments (following Haskell compiler approach)
                let resolved_args = self.resolve_default_args(&variant_info, &enum_def, args)?;

                // Update the args in the expression with resolved defaults
                *args = resolved_args;

                // Validate argument count matches (after defaults are resolved)
                if args.len() != variant_info.arg_types.len() {
                    return Err(CompilationError::TypeError(format!(
                        "Enum variant '{}.{}' expects {} arguments, got {}",
                        enum_name,
                        variant_name,
                        variant_info.arg_types.len(),
                        args.len()
                    )));
                }

                // Infer types for all arguments and unify with expected types
                for (arg, &expected_ty) in args.iter_mut().zip(variant_info.arg_types.iter()) {
                    let arg_ty = self.infer_expr(arg)?;
                    self.unify(arg_ty, expected_ty)?;
                }

                // Create a named type for the enum
                let enum_ty = self.store.new_known(Type::Named(enum_name.clone()));
                *ty = enum_ty;
                enum_ty
            }
        };

        Ok(ty)
    }

    /// Resolve default arguments for enum variant constructors.
    /// Returns a new Vec with default values filled in.
    /// Follows the Haskell compiler's `addDefaultArgs` function.
    fn resolve_default_args(
        &self,
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
                    CompilationError::TypeError(format!(
                        "Variant '{}' not found in enum '{}'",
                        variant_info.variant_name, variant_info.enum_name
                    ))
                })?;

            let provided_len = result.len();
            for i in (0..total_required).rev() {
                if i >= provided_len
                    && let Some(default_expr) = variant.args.get(i).and_then(|f| f.default.as_ref())
                {
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
