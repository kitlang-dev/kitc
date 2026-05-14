use crate::codegen::module::ModulePath;

/// Mangle a name with its module path prefix for C code generation.
/// Empty module paths produce unmodified names (for backward compatibility).
pub fn mangle_name(module_path: &ModulePath, name: &str) -> String {
    if module_path.is_empty() {
        name.to_string()
    } else {
        let prefix = module_path.join("_");
        format!("{}_{}", prefix, name)
    }
}

/// Mangle a function name with its module path.
pub fn mangle_function(module_path: &ModulePath, name: &str) -> String {
    mangle_name(module_path, name)
}

/// Mangle a global variable name with its module path.
pub fn mangle_global(module_path: &ModulePath, name: &str) -> String {
    mangle_name(module_path, name)
}

/// Mangle a type name with its module path.
pub fn mangle_type(module_path: &ModulePath, name: &str) -> String {
    mangle_name(module_path, name)
}

/// Mangle an enum variant name: combines enum name and variant name, then mangles with module path.
pub fn mangle_enum_variant(
    module_path: &ModulePath,
    enum_name: &str,
    variant_name: &str,
) -> String {
    let full_name = format!("{}_{}", enum_name, variant_name);
    mangle_name(module_path, &full_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mangle_empty_module() {
        let path = ModulePath::new();
        assert_eq!(mangle_name(&path, "foo"), "foo");
    }

    #[test]
    fn test_mangle_single_module() {
        let path = ModulePath(vec!["utils".to_string()]);
        assert_eq!(mangle_name(&path, "foo"), "utils_foo");
    }

    #[test]
    fn test_mangle_nested_module() {
        let path = ModulePath(vec!["pkg".to_string(), "util".to_string()]);
        assert_eq!(mangle_name(&path, "foo"), "pkg_util_foo");
    }

    #[test]
    fn test_mangle_enum_variant() {
        let path = ModulePath(vec!["myapp".to_string()]);
        assert_eq!(
            mangle_enum_variant(&path, "Status", "Active"),
            "myapp_Status_Active"
        );
    }
}
