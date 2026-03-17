use super::type_ast::{EnumDefinition, EnumVariant, Field, StructDefinition};
use super::types::TypeId;
use std::collections::HashMap;

/// Stores information about an enum variant for lookup.
#[derive(Clone, Debug)]
pub struct EnumVariantInfo {
    pub enum_name: String,
    pub variant_name: String,
    pub arg_types: Vec<TypeId>,
    pub has_defaults: bool,
}

/// Symbol table for tracking variable and function types during inference.
///
/// Currently uses a flat scope (no nesting). Variables and functions are tracked
/// by their names and their `TypeId`s.
pub struct SymbolTable {
    /// Maps global variable names to their inferred `TypeId`s.
    globals: HashMap<String, TypeId>,

    /// Maps local variable names to their inferred `TypeId`s.
    vars: HashMap<String, TypeId>,

    /// Maps function names to their signatures (parameter types, return type).
    functions: HashMap<String, (Vec<TypeId>, TypeId)>,

    /// Maps struct names to their definitions.
    structs: HashMap<String, StructDefinition>,

    /// Maps enum names to their definitions.
    enums: HashMap<String, EnumDefinition>,

    /// Maps qualified variant names ("EnumName.VariantName") to variant info.
    enum_variants: HashMap<String, EnumVariantInfo>,
}

impl Default for SymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolTable {
    pub fn new() -> Self {
        Self {
            globals: HashMap::new(),
            vars: HashMap::new(),
            functions: HashMap::new(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            enum_variants: HashMap::new(),
        }
    }

    /// Define a global variable in the symbol table.
    pub fn define_global(&mut self, name: &str, ty: TypeId) {
        self.globals.insert(name.to_string(), ty);
    }

    /// Look up a global variable's type.
    pub fn lookup_global(&self, name: &str) -> Option<TypeId> {
        self.globals.get(name).copied()
    }

    /// Define a variable in the current scope.
    pub fn define_var(&mut self, name: &str, ty: TypeId) {
        self.vars.insert(name.to_string(), ty);
    }

    /// Look up a variable's type.
    pub fn lookup_var(&self, name: &str) -> Option<TypeId> {
        self.vars.get(name).copied()
    }

    /// Define a function signature.
    pub fn define_function(&mut self, name: &str, params: Vec<TypeId>, ret: TypeId) {
        self.functions.insert(name.to_string(), (params, ret));
    }

    /// Look up a function's signature.
    pub fn lookup_function(&self, name: &str) -> Option<(Vec<TypeId>, TypeId)> {
        self.functions.get(name).cloned()
    }

    /// Define a struct type.
    pub fn define_struct(&mut self, def: StructDefinition) {
        self.structs.insert(def.name.clone(), def);
    }

    /// Look up a struct definition by name.
    pub fn lookup_struct(&self, name: &str) -> Option<&StructDefinition> {
        self.structs.get(name)
    }

    /// Look up a field in a struct by struct and field name.
    pub fn lookup_struct_field(&self, struct_name: &str, field_name: &str) -> Option<&Field> {
        self.structs
            .get(struct_name)
            .and_then(|s| s.fields.iter().find(|f| f.name == field_name))
    }

    /// Define an enum type.
    pub fn define_enum(&mut self, def: EnumDefinition) {
        self.enums.insert(def.name.clone(), def);
    }

    /// Look up an enum definition by name.
    pub fn lookup_enum(&self, name: &str) -> Option<&EnumDefinition> {
        self.enums.get(name)
    }

    /// Define an enum variant constructor.
    pub fn define_enum_variant(&mut self, variant: &EnumVariant) {
        let qualified_name = format!("{}.{}", variant.parent, variant.name);
        let has_defaults = variant.args.iter().any(|f| f.default.is_some());
        let arg_types: Vec<TypeId> = variant.args.iter().map(|f| f.ty).collect();

        self.enum_variants.insert(
            qualified_name,
            EnumVariantInfo {
                enum_name: variant.parent.clone(),
                variant_name: variant.name.clone(),
                arg_types,
                has_defaults,
            },
        );
    }

    /// Look up an enum variant by qualified name ("EnumName.VariantName").
    pub fn lookup_enum_variant(&self, qualified_name: &str) -> Option<&EnumVariantInfo> {
        self.enum_variants.get(qualified_name)
    }

    /// Look up an enum variant by simple name across all enums.
    pub fn lookup_enum_variant_by_simple_name(
        &self,
        simple_name: &str,
    ) -> Option<&EnumVariantInfo> {
        self.enum_variants
            .values()
            .find(|v| v.variant_name == simple_name)
    }

    /// Look up an enum variant by enum name and variant name.
    pub fn lookup_variant(&self, enum_name: &str, variant_name: &str) -> Option<&EnumVariantInfo> {
        let qualified_name = format!("{}.{}", enum_name, variant_name);
        self.enum_variants.get(&qualified_name)
    }

    /// Get all variant names for an enum.
    pub fn get_enum_variants(&self, enum_name: &str) -> Vec<&EnumVariantInfo> {
        self.enum_variants
            .values()
            .filter(|v| v.enum_name == enum_name)
            .collect()
    }

    /// Get all registered enums.
    pub fn get_enums(&self) -> Vec<&EnumDefinition> {
        self.enums.values().collect()
    }
}
