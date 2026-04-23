use crate::codegen::ast::ModulePath;

pub fn mangle_name(module_path: &ModulePath, name: &str) -> String {
    // Disabled for backward compatibility - will re-enable when module system is properly used
    name.to_string()
}

pub fn mangle_function(module_path: &ModulePath, name: &str) -> String {
    mangle_name(module_path, name)
}

pub fn mangle_global(module_path: &ModulePath, name: &str) -> String {
    mangle_name(module_path, name)
}

pub fn mangle_type(module_path: &ModulePath, name: &str) -> String {
    mangle_name(module_path, name)
}

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
        assert_eq!(mangle_name(&path, "foo"), "foo");
    }

    #[test]
    fn test_mangle_nested_module() {
        let path = ModulePath(vec!["pkg".to_string(), "util".to_string()]);
        assert_eq!(mangle_name(&path, "foo"), "foo");
    }

    #[test]
    fn test_mangle_enum_variant() {
        let path = ModulePath(vec!["myapp".to_string()]);
        assert_eq!(
            mangle_enum_variant(&path, "Status", "Active"),
            "Status_Active"
        );
    }
}
