use std::path::{Path, PathBuf};

use kitc_common::{get_builtin_headers, get_system_include_dirs};
use kitc_ffi::types::{CDeclarations, CQualifier, CType, MacroValue};
use kitc_ffi::{PreprocessConfig, extract_header, extract_header_from_source};

use super::inference::TypeInferencer;
use super::type_ast::{EnumDefinition, EnumVariant, Field, StructDefinition};
use super::types::{Type, TypeId};

use crate::codegen::Include;
use crate::error::{CompilationError, CompileResult};

/// Register C header declarations into the Kit compiler's type system.
///
/// Uses default configuration with builtin system headers enabled.
/// For custom configuration, use `register_c_header_with_config`.
///
/// # Errors
///
/// Returns `CompilationError` if header preprocessing or declaration registration fails.
pub fn register_c_header(header_path: &str, inferencer: &mut TypeInferencer) -> CompileResult<()> {
    let config = PreprocessConfig::new()
        .with_builtin_headers(true)
        .with_current_target();
    register_c_header_with_config(header_path, config, inferencer)
}

/// Register C header declarations with a custom preprocessor configuration.
///
/// # Errors
///
/// Returns `CompilationError` if header extraction or declaration registration fails.
pub fn register_c_header_with_config(
    header_path: &str,
    config: PreprocessConfig,
    inferencer: &mut TypeInferencer,
) -> CompileResult<()> {
    let decls = extract_header(header_path, &config).map_err(|e| {
        CompilationError::CompileError(format!("Failed to process C header '{}': {e}", header_path))
    })?;

    // Surface declarations the header parser had to skip so users can diagnose
    // missing symbols that are actually unparsed C rather than unknown Kit APIs.
    for skipped in &decls.skipped_nodes {
        log::warn!(
            "C header '{}': skipped {} at line {} column {} due to parse errors",
            header_path,
            skipped.kind,
            skipped.line,
            skipped.column
        );
    }

    register_declarations(decls, inferencer)
}

/// Register declarations from a builtin system header by name.
fn register_builtin_header(
    header_name: &str,
    inferencer: &mut TypeInferencer,
) -> CompileResult<()> {
    let content = get_builtin_headers()
        .get(header_name)
        .copied()
        .ok_or_else(|| {
            crate::error::CompilationError::CompileError(format!(
                "No builtin header available for '{}'",
                header_name
            ))
        })?;

    let config = PreprocessConfig::new()
        .with_builtin_headers(true)
        .with_current_target();

    let decls = extract_header_from_source(content, &config).map_err(|e| {
        CompilationError::CompileError(format!(
            "Failed to process builtin header '{}': {e}",
            header_name
        ))
    })?;

    register_declarations(decls, inferencer)
}

/// Register declarations from pre-parsed C declarations.
///
/// # Errors
///
/// Returns `CompilationError` if struct/enum/function registration fails.
pub fn register_declarations(
    decls: CDeclarations,
    inferencer: &mut TypeInferencer,
) -> CompileResult<()> {
    // 1. Register typedefs first (types may reference them)
    for td in &decls.typedefs {
        let kit_type = ctype_to_kit(&td.underlying, &decls);
        inferencer.store.register_typedef(td.name.clone(), kit_type);
    }

    // 2. Register struct types
    for s in &decls.structs {
        let mut fields = Vec::new();
        for field in &s.fields {
            let field_ty = ctype_to_kit(&field.ty, &decls);
            let field_type_id = inferencer.store.new_known(field_ty.clone());
            fields.push(Field {
                name: field.name.clone().unwrap_or_default(),
                ty: field_type_id,
                annotation: Some(field_ty),
                is_const: false,
                default: None,
            });
        }

        let struct_def = StructDefinition {
            name: s.name.clone(),
            type_params: vec![],
            fields,
            is_public: true,
            metadata: vec![],
        };

        let field_types: Vec<(String, TypeId)> = struct_def
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.ty))
            .collect();

        let struct_type = Type::Struct {
            name: struct_def.name.clone(),
            fields: field_types,
        };

        inferencer.store.new_known(struct_type);
        inferencer.symbols_mut().define_struct(struct_def);
        inferencer.mark_imported_struct(s.name.clone());
    }

    // 3. Register union types (as structs with all fields)
    for u in &decls.unions {
        let mut fields = Vec::new();
        for field in &u.fields {
            let field_ty = ctype_to_kit(&field.ty, &decls);
            let field_type_id = inferencer.store.new_known(field_ty.clone());
            fields.push(Field {
                name: field.name.clone().unwrap_or_default(),
                ty: field_type_id,
                annotation: Some(field_ty),
                is_const: false,
                default: None,
            });
        }

        let struct_def = StructDefinition {
            name: u.name.clone(),
            type_params: vec![],
            fields,
            is_public: true,
            metadata: vec![],
        };

        let field_types: Vec<(String, TypeId)> = struct_def
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.ty))
            .collect();

        let struct_type = Type::Struct {
            name: struct_def.name.clone(),
            fields: field_types,
        };

        inferencer.store.new_known(struct_type);
        inferencer.symbols_mut().define_struct(struct_def);
        inferencer.mark_imported_struct(u.name.clone());
    }

    // 4. Register enum types
    for e in &decls.enums {
        let mut enum_def = EnumDefinition {
            name: e.name.clone(),
            type_params: vec![],
            variants: vec![],
            is_public: true,
            metadata: vec![],
        };

        for variant in &e.variants {
            let enum_variant = EnumVariant {
                name: variant.name.clone(),
                parent: e.name.clone(),
                args: vec![],
                default: None,
                metadata: vec![],
            };
            enum_def.variants.push(enum_variant);
        }

        inferencer.symbols_mut().define_enum(enum_def.clone());

        for variant in &enum_def.variants {
            inferencer.symbols_mut().define_enum_variant(variant);
        }
    }

    // 5. Register function signatures (skip variadic - they fall through to the
    //    existing "unknown C function" path in inference, which handles variadic calls)
    for func in &decls.functions {
        if func.is_variadic {
            log::debug!(
                "Skipping variadic C function '{}' (handled by inference fallback)",
                func.name
            );
            continue;
        }

        let ret_type_id = inferencer
            .store
            .new_known(ctype_to_kit(&func.return_type, &decls));

        let param_ids: Vec<TypeId> = func
            .params
            .iter()
            .map(|p| inferencer.store.new_known(ctype_to_kit(&p.ty, &decls)))
            .collect();

        inferencer
            .symbols_mut()
            .define_function(&func.name, param_ids, ret_type_id);

        // Also register as a global (for higher-order usage)
        let param_tys: Vec<Type> = func
            .params
            .iter()
            .map(|p| ctype_to_kit(&p.ty, &decls))
            .collect();
        let ret_ty = ctype_to_kit(&func.return_type, &decls);
        let fn_ty = Type::Function {
            param_tys,
            ret_ty: Box::new(ret_ty),
        };
        let fn_ty_id = inferencer.store.new_known(fn_ty);
        inferencer.symbols_mut().define_global(&func.name, fn_ty_id);
    }

    // 6. Register global variables
    for g in &decls.globals {
        let ty = ctype_to_kit(&g.ty, &decls);
        let type_id = inferencer.store.new_known(ty);
        inferencer.symbols_mut().define_global(&g.name, type_id);
    }

    // 7. Register macro constants as globals
    for mc in &decls.macro_constants {
        let ty = match mc.value {
            MacroValue::Int(_) => Type::Int,
            MacroValue::Uint(_) => Type::Uint64,
            MacroValue::Float(_) => Type::Float,
            MacroValue::String(_) => Type::CString,
        };
        let type_id = inferencer.store.new_known(ty);
        inferencer.symbols_mut().define_global(&mc.name, type_id);
    }

    Ok(())
}

/// Convert a `CType` to a Kit Type.
fn ctype_to_kit(ct: &CType, decls: &CDeclarations) -> Type {
    let resolved = ct.resolve_typedef(decls);

    match resolved {
        CType::Void => Type::Void,
        CType::Char => Type::Char,
        CType::Short => Type::Int16,
        CType::Int => Type::Int,
        CType::Long => Type::Int64,
        CType::LongLong => Type::Int64,
        CType::Float => Type::Float,
        CType::Double => Type::Float64,
        CType::LongDouble => Type::Float64,
        CType::Bool => Type::Bool,
        CType::SignedChar => Type::Int8,
        CType::UnsignedChar => Type::Uint8,
        CType::UnsignedShort => Type::Uint16,
        CType::UnsignedInt => Type::Uint32,
        CType::UnsignedLong => Type::Uint64,
        CType::UnsignedLongLong => Type::Uint64,

        CType::Int8 => Type::Int8,
        CType::Int16 => Type::Int16,
        CType::Int32 => Type::Int32,
        CType::Int64 => Type::Int64,
        CType::Uint8 => Type::Uint8,
        CType::Uint16 => Type::Uint16,
        CType::Uint32 => Type::Uint32,
        CType::Uint64 => Type::Uint64,

        CType::SizeT => Type::Size,
        CType::SSizeT => Type::Int64,
        CType::IntPtr => Type::Int64,
        CType::UintPtr => Type::Uint64,
        CType::PtrDiffT => Type::Int64,

        CType::Named(name) => {
            if name == "FILE" || name == "va_list" {
                Type::Ptr(Box::new(Type::Void))
            } else if name == "char" {
                Type::Char
            } else {
                Type::Named(name.clone())
            }
        }

        CType::Ptr(inner, qualifiers) => {
            let is_const = qualifiers.contains(&CQualifier::Const);
            match inner.as_ref() {
                CType::Char if is_const => Type::CString,
                CType::Void => Type::Ptr(Box::new(Type::Void)),
                _ => {
                    let inner_kit = ctype_to_kit(inner, decls);
                    Type::Ptr(Box::new(inner_kit))
                }
            }
        }

        CType::FunctionPtr { .. } => Type::Ptr(Box::new(Type::Void)),

        CType::Array { element_type, size } => {
            let elem = ctype_to_kit(element_type, decls);
            Type::CArray(Box::new(elem), size.unwrap_or(0))
        }

        CType::Unknown(name) => Type::Named(name.clone()),
    }
}

/// Helper to register all includes from a module into the inferencer.
///
/// # Errors
///
/// Returns `CompilationError` if a header cannot be resolved or registered.
pub fn register_module_includes(
    includes: &[Include],
    source_path: &Path,
    inferencer: &mut TypeInferencer,
) -> CompileResult<()> {
    let source_dir = source_path.parent().unwrap_or(Path::new("."));

    // System include directories to search for C headers.
    //
    // Always fall back to the conventional Unix locations, then append whatever directories the
    // active C toolchain reports (e.g. MSVC's `INCLUDE` and the Windows SDK `ucrt`/`um` dirs on
    // Windows).
    let mut system_dirs: Vec<PathBuf> = vec![
        PathBuf::from("/usr/include"),
        PathBuf::from("/usr/local/include"),
    ];
    system_dirs.extend(get_system_include_dirs());

    for inc in includes {
        let header_name = inc.path.trim_start_matches('/');

        // Try to find the header file on disk
        let mut candidate_paths = vec![
            source_dir.join(header_name),
            Path::new(header_name).to_path_buf(),
        ];
        for dir in &system_dirs {
            candidate_paths.push(dir.join(header_name));
        }

        let found = candidate_paths.iter().find(|p| p.exists());
        if let Some(path) = found {
            let path_str = path.to_string_lossy().to_string();
            log::info!("Registering C header: {}", path_str);
            if let Err(e) = register_c_header(&path_str, inferencer) {
                log::warn!("Failed to register C header '{}': {e}", inc.path);
            }
        } else if get_builtin_headers().contains_key(header_name) {
            log::info!("Using builtin system header for '{}'", header_name);
            if let Err(e) = register_builtin_header(header_name, inferencer) {
                log::warn!("Failed to register builtin header '{}': {e}", header_name);
            }
        } else {
            log::warn!(
                "C header '{}' not found! Declarations from it will be unavailable",
                inc.path
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::inference::TypeInferencer;

    #[test]
    fn test_ctype_to_kit_primitives() {
        let decls = CDeclarations::default();

        assert_eq!(ctype_to_kit(&CType::Void, &decls), Type::Void);
        assert_eq!(ctype_to_kit(&CType::Int, &decls), Type::Int);
        assert_eq!(ctype_to_kit(&CType::Char, &decls), Type::Char);
        assert_eq!(ctype_to_kit(&CType::Float, &decls), Type::Float);
        assert_eq!(ctype_to_kit(&CType::Double, &decls), Type::Float64);
        assert_eq!(ctype_to_kit(&CType::Bool, &decls), Type::Bool);
        assert_eq!(ctype_to_kit(&CType::Short, &decls), Type::Int16);
        assert_eq!(ctype_to_kit(&CType::Long, &decls), Type::Int64);
        assert_eq!(ctype_to_kit(&CType::SizeT, &decls), Type::Size);
    }

    #[test]
    fn test_ctype_to_kit_pointer() {
        let decls = CDeclarations::default();

        let int_ptr = CType::Ptr(Box::new(CType::Int), vec![]);
        assert_eq!(
            ctype_to_kit(&int_ptr, &decls),
            Type::Ptr(Box::new(Type::Int))
        );

        let const_char_ptr = CType::Ptr(Box::new(CType::Char), vec![CQualifier::Const]);
        assert_eq!(ctype_to_kit(&const_char_ptr, &decls), Type::CString);

        let void_ptr = CType::Ptr(Box::new(CType::Void), vec![]);
        assert_eq!(
            ctype_to_kit(&void_ptr, &decls),
            Type::Ptr(Box::new(Type::Void))
        );
    }

    #[test]
    fn test_ctype_to_kit_array() {
        let decls = CDeclarations::default();

        let arr = CType::Array {
            element_type: Box::new(CType::Int),
            size: Some(10),
        };
        assert_eq!(
            ctype_to_kit(&arr, &decls),
            Type::CArray(Box::new(Type::Int), 10)
        );

        let unsized_arr = CType::Array {
            element_type: Box::new(CType::Char),
            size: None,
        };
        assert_eq!(
            ctype_to_kit(&unsized_arr, &decls),
            Type::CArray(Box::new(Type::Char), 0)
        );
    }

    #[test]
    fn test_ctype_to_kit_named() {
        let decls = CDeclarations::default();

        let named = CType::Named("MyStruct".to_string());
        assert_eq!(
            ctype_to_kit(&named, &decls),
            Type::Named("MyStruct".to_string())
        );

        let file = CType::Named("FILE".to_string());
        assert_eq!(ctype_to_kit(&file, &decls), Type::Ptr(Box::new(Type::Void)));
    }

    #[test]
    fn test_register_function_declarations() {
        let mut inferencer = TypeInferencer::new();
        let mut decls = CDeclarations::default();

        decls.functions.push(CFunction {
            name: "my_func".to_string(),
            return_type: CType::Int,
            params: vec![CParam {
                name: Some("x".to_string()),
                ty: CType::Int,
            }],
            is_variadic: false,
        });

        let result = register_declarations(decls, &mut inferencer);
        assert!(result.is_ok());

        let sig = inferencer.symbols().lookup_function("my_func");
        assert!(
            sig.is_some(),
            "Function should be registered in symbol table"
        );
    }

    #[test]
    fn test_register_typedefs() {
        let mut inferencer = TypeInferencer::new();
        let mut decls = CDeclarations::default();

        decls.typedefs.push(CTypedef {
            name: "myint".to_string(),
            underlying: CType::Int,
        });

        let result = register_declarations(decls, &mut inferencer);
        assert!(result.is_ok());

        assert_eq!(inferencer.store.typedefs.len(), 1);
        assert_eq!(inferencer.store.typedefs[0].0, "myint");
    }

    #[test]
    fn test_register_globals() {
        let mut inferencer = TypeInferencer::new();
        let mut decls = CDeclarations::default();

        decls.globals.push(CGlobalVar {
            name: "global_x".to_string(),
            ty: CType::Int,
            is_const: false,
        });

        let result = register_declarations(decls, &mut inferencer);
        assert!(result.is_ok());

        let ty = inferencer.symbols().lookup_global("global_x");
        assert!(ty.is_some(), "Global should be registered in symbol table");
    }
}
