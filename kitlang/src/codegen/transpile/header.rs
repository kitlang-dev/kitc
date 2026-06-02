use std::collections::HashSet;
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;

use crate::codegen::ast::{Attributed, Program};
use crate::codegen::module::{Module, ModulePath};
use crate::codegen::name_mangling::mangle_name;
use crate::codegen::types::ToCRepr;
use crate::error::{CompilationError, CompileResult};

use super::CodegenCtx;
use super::collect_type_headers_and_decls;

/// Pre-collected name sets for a module's declarations.
struct NameSets {
    functions: HashSet<String>,
    globals: HashSet<String>,
    structs: HashSet<String>,
    enums: HashSet<String>,
}

impl NameSets {
    fn from_module(module: &Module) -> Self {
        Self {
            functions: module
                .program
                .functions
                .iter()
                .map(|f| f.name.clone())
                .collect(),
            globals: module
                .program
                .globals
                .iter()
                .map(|g| g.name.clone())
                .collect(),
            structs: module
                .program
                .structs
                .iter()
                .map(|s| s.name.clone())
                .collect(),
            enums: module
                .program
                .enums
                .iter()
                .map(|e| e.name.clone())
                .collect(),
        }
    }
}

/// Filter items from `source` that have a name in `names`.
fn filter_by_name<T: Clone>(
    source: &[T],
    names: &HashSet<String>,
    get_name: fn(&T) -> &str,
) -> Vec<T> {
    source
        .iter()
        .filter(|item| names.contains(get_name(item)))
        .cloned()
        .collect()
}

impl CodegenCtx<'_> {
    /// Generate per-module `.c` and `.h` files, returning paths to all `.c` files.
    pub(crate) fn generate_per_module_files(
        &mut self,
        sorted_paths: &[ModulePath],
        merged: &Program,
    ) -> CompileResult<Vec<PathBuf>> {
        fs::create_dir_all(self.build_dir).map_err(CompilationError::Io)?;

        let mut c_files = Vec::new();

        for path in sorted_paths {
            self.current_module = path.clone();
            if let Some(module) = self.registry.get(path) {
                let names = NameSets::from_module(module);

                let filtered = Program {
                    module_path: Some(path.clone()),
                    globals: filter_by_name(&merged.globals, &names.globals, |g| &g.name),
                    functions: filter_by_name(&merged.functions, &names.functions, |f| &f.name),
                    structs: filter_by_name(&merged.structs, &names.structs, |s| &s.name),
                    enums: filter_by_name(&merged.enums, &names.enums, |e| &e.name),
                    traits: vec![],
                    impls: vec![],
                    rulesets: vec![],
                    typedefs: vec![],
                };

                let header = self.generate_module_header_from_program(&filtered, module);
                let h_name = format!("{}.h", path.join("_"));
                fs::write(self.build_dir.join(&h_name), header).map_err(CompilationError::Io)?;

                let c_code = self.generate_module_c_code_from_program(&filtered, module);
                let c_name = format!("{}.c", path.join("_"));
                let c_path = self.build_dir.join(&c_name);
                fs::write(&c_path, c_code).map_err(CompilationError::Io)?;

                c_files.push(c_path);
            }
        }

        Ok(c_files)
    }

    /// Generate a header file for a module using the inferred program data.
    pub(crate) fn generate_module_header_from_program(
        &self,
        prog: &Program,
        module: &Module,
    ) -> String {
        let mut out = String::new();
        let guard = format!("KIT_MODULE_{}_H", module.path.join("_").to_uppercase());
        let _ = writeln!(out, "#ifndef {}", guard);
        let _ = writeln!(out, "#define {}", guard);
        out.push('\n');

        let seen_headers = collect_type_headers_and_decls(self.inferencer, prog).0;
        for hdr in &seen_headers {
            let _ = writeln!(out, "#include {hdr}");
        }
        out.push('\n');

        for import in &module.imports {
            if self.registry.contains(&import.path) {
                let dep = format!("{}.h", import.path.join("_"));
                let _ = writeln!(out, "#include \"{}\"", dep);
            }
        }
        if !module.imports.is_empty() {
            out.push('\n');
        }

        for struct_def in &prog.structs {
            out.push_str(&self.generate_struct_declaration(struct_def, &prog.structs));
            out.push('\n');
        }

        for enum_def in &prog.enums {
            out.push_str(&self.generate_enum_declaration(enum_def));
            out.push('\n');
        }

        for global in &prog.globals {
            if global.is_public || global.is_extern() {
                let ty = match self.inferencer.store.resolve(global.inferred) {
                    Ok(t) => self.type_to_c_name_with_module(&t, &module.path),
                    Err(_) => global
                        .annotation
                        .as_ref()
                        .map_or_else(|| "int".to_string(), |a| a.to_c_repr().name),
                };
                let mod_path = global.mangling_module(&module.path);
                let gname = mangle_name(&mod_path, &global.name);
                let const_ = if global.is_const { "const " } else { "" };
                let _ = writeln!(out, "extern {const_}{ty} {};", gname);
            }
        }
        if prog.globals.iter().any(|g| g.is_public || g.is_extern()) {
            out.push('\n');
        }

        for func in &prog.functions {
            let ret = self.resolve_return_type_c_name(func);
            let mod_path = func.mangling_module(&module.path);
            let fname = if func.name == "main" {
                "main".to_string()
            } else {
                mangle_name(&mod_path, &func.name)
            };
            let params = self.format_function_params_with_module(&func.params, &module.path);
            let extern_prefix = if func.is_extern() { "extern " } else { "" };
            let _ = writeln!(out, "{extern_prefix}{} {}({});", ret, fname, params);
        }

        out.push('\n');
        let _ = writeln!(out, "#endif /* {} */", guard);
        out
    }

    /// Generate a per-module C source file using the filtered (inferred) program data.
    fn generate_module_c_code_from_program(&self, prog: &Program, module: &Module) -> String {
        let mut out = String::new();
        let header = format!("{}.h", module.path.join("_"));
        let _ = writeln!(out, "#include \"{}\"", header);

        for import in &module.imports {
            if self.registry.contains(&import.path) {
                let dep = format!("{}.h", import.path.join("_"));
                let _ = writeln!(out, "#include \"{}\"", dep);
            }
        }
        for inc in &module.includes {
            let _ = writeln!(out, "#include \"{}\"", inc.path);
        }
        out.push('\n');

        let (seen_headers, _) = collect_type_headers_and_decls(self.inferencer, prog);
        for hdr in &seen_headers {
            let _ = writeln!(out, "#include {hdr}");
        }
        if !seen_headers.is_empty() {
            out.push('\n');
        }

        for global in &prog.globals {
            out.push_str(&self.transpile_global(global));
            out.push('\n');
        }
        for func in &prog.functions {
            out.push_str(&self.transpile_function(func));
            out.push_str("\n\n");
        }
        out
    }
}
