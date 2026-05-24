use std::fmt::Write;

use crate::codegen::ast::Attributed;
use crate::codegen::frontend::Compiler;
use crate::codegen::module::ModulePath;
use crate::codegen::name_mangling::{mangle_enum_variant, mangle_name};
use crate::codegen::type_ast::{EnumDefinition, EnumVariant, Field, StructDefinition};
use crate::codegen::types::{ToCRepr, Type};

impl Compiler {
    fn resolve_field_type(&self, field: &Field) -> Type {
        self.inferencer
            .store
            .resolve(field.ty)
            .ok()
            .or(field.annotation.clone())
            .unwrap_or(Type::Void)
    }

    /// Generate a C struct declaration from a Kit struct definition.
    pub(super) fn generate_struct_declaration(
        &self,
        struct_def: &StructDefinition,
        _all_structs: &[StructDefinition],
    ) -> String {
        let field_decls: Vec<String> = struct_def
            .fields
            .iter()
            .map(|field| {
                let ty = self.resolve_field_type(field);
                let prefix = if field.is_const { "const " } else { "" };
                let cname = self.type_to_c_name(&ty);
                format!("    {}{} {};", prefix, cname, field.name)
            })
            .collect();

        let module = struct_def.mangling_module(&self.current_module);
        let struct_name = mangle_name(&module, &struct_def.name);
        format!("struct {} {{\n{}\n}};", struct_name, field_decls.join("\n"))
    }

    /// Generate a C enum declaration from a Kit enum definition.
    /// Simple enums (no data variants) become plain C `enum`s.
    /// Enums with data-carrying variants get a tagged-union layout.
    pub(super) fn generate_enum_declaration(&self, enum_def: &EnumDefinition) -> String {
        let enum_module = enum_def.mangling_module(&self.current_module);
        let variant_module = |v: &EnumVariant| -> ModulePath { v.mangling_module(&enum_module) };

        let mut output = String::new();
        let enum_type_name = mangle_name(&enum_module, &enum_def.name);
        let all_simple = enum_def.variants.iter().all(|v| v.args.is_empty());

        if all_simple {
            let variants: Vec<String> = enum_def
                .variants
                .iter()
                .map(|v| {
                    let mp = variant_module(v);
                    format!("    {}", mangle_enum_variant(&mp, &enum_def.name, &v.name))
                })
                .collect();

            let _ = write!(
                output,
                "typedef enum {{\n{}\n}} {};\n\n",
                variants.join(",\n"),
                enum_type_name
            );
        } else {
            let disc: Vec<String> = enum_def
                .variants
                .iter()
                .map(|v| {
                    let mp = variant_module(v);
                    format!("    {}", mangle_enum_variant(&mp, &enum_def.name, &v.name))
                })
                .collect();
            let _ = write!(
                output,
                "typedef enum {{\n{}\n}} {}_Discriminant;\n\n",
                disc.join(",\n"),
                enum_type_name
            );

            for v in enum_def.variants.iter().filter(|v| !v.args.is_empty()) {
                let fields: Vec<String> = v
                    .args
                    .iter()
                    .map(|arg| {
                        let ty = self.resolve_field_type(arg);
                        format!("    {} {};", ty.to_c_repr().name, arg.name)
                    })
                    .collect();
                let _ = write!(
                    output,
                    "typedef struct {{\n{}\n}} {}_{}_data;\n\n",
                    fields.join("\n"),
                    enum_type_name,
                    v.name
                );
            }

            let union_fields: Vec<String> = enum_def
                .variants
                .iter()
                .filter(|v| !v.args.is_empty())
                .map(|v| {
                    format!(
                        "    {}_{}_data {};",
                        enum_type_name,
                        v.name,
                        v.name.to_lowercase()
                    )
                })
                .collect();

            let body = format!(
                "    {}_Discriminant _discriminant;\n    union {{\n{}\n    }} _variant;",
                enum_type_name,
                union_fields.join("\n")
            );
            let _ = write!(
                output,
                "typedef struct {{\n{}\n}} {};\n\n",
                body, enum_type_name
            );
        }

        for v in enum_def.variants.iter().filter(|v| !v.args.is_empty()) {
            let params: Vec<String> = v
                .args
                .iter()
                .map(|arg| {
                    let ty = self.resolve_field_type(arg);
                    format!("{} {}", ty.to_c_repr().name, arg.name)
                })
                .collect();
            let arg_names: Vec<String> = v.args.iter().map(|arg| arg.name.clone()).collect();
            let assigns: Vec<String> = v
                .args
                .iter()
                .enumerate()
                .map(|(i, arg)| {
                    format!(
                        "    result._variant.{}.{} = {};",
                        v.name.to_lowercase(),
                        arg.name,
                        arg_names[i]
                    )
                })
                .collect();
            let mp = variant_module(v);
            let ctor = mangle_enum_variant(&mp, &enum_def.name, &v.name);
            let _ = write!(
                output,
                "{} {}_new({}) {{\n    {} result;\n    result._discriminant = {};\n{}\n    return result;\n}}\n\n",
                enum_type_name,
                ctor,
                params.join(", "),
                enum_type_name,
                ctor,
                assigns.join("\n")
            );
        }

        output
    }
}
