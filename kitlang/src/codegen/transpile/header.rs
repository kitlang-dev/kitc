use std::collections::HashSet;
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;

use crate::codegen::ast::Program;
use crate::codegen::frontend::{Compiler, merge_modules_for_inference};
use crate::codegen::module::{Module, ModulePath};
use crate::codegen::name_mangling::{mangle_function, mangle_global};
use crate::codegen::types::ToCRepr;
use crate::error::{CompilationError, CompileResult};

use super::collect_type_headers_and_decls;

impl Compiler {
    /// Generate C code from the merged program and write it to the flat output path.
    pub(crate) fn transpile_with_program(&mut self, prog: &Program) -> CompileResult<()> {
        let c_code = self.generate_flat_c_code(prog);
        fs::write(&self.c_output, c_code).map_err(CompilationError::Io)
    }

    /// Generate C code for a single merged/entry program (flat, no module awareness).
    pub(super) fn generate_flat_c_code(&self, prog: &Program) -> String {
        let mut out = String::new();

        let mut all_c_includes = HashSet::new();
        for module in self.registry.all_modules() {
            for inc in &module.includes {
                all_c_includes.insert(inc.path.clone());
            }
        }
        for path in &all_c_includes {
            writeln!(out, "#include \"{}\"", path).unwrap();
        }
        if !all_c_includes.is_empty() {
            out.push('\n');
        }

        let (seen_headers, seen_declarations) =
            collect_type_headers_and_decls(&self.inferencer, prog);

        for hdr in &seen_headers {
            writeln!(out, "#include {hdr}").unwrap();
        }
        out.push('\n');

        for decl in &seen_declarations {
            out.push_str(decl);
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
            out.push_str(&self.transpile_global(global));
            out.push('\n');
        }

        for func in &prog.functions {
            out.push_str(&self.transpile_function(func));
            out.push_str("\n\n");
        }
        out
    }

    /// Generate per-module `.c` and `.h` files, returning paths to all `.c` files.
    pub(crate) fn generate_per_module_files(
        &mut self,
        sorted_paths: &[ModulePath],
    ) -> CompileResult<Vec<PathBuf>> {
        fs::create_dir_all(&self.build_dir).map_err(CompilationError::Io)?;

        let mut c_files = Vec::new();
        let saved_module = self.current_module.clone();

        let mut merged = merge_modules_for_inference(&self.registry, sorted_paths);
        self.inferencer.infer_program(&mut merged)?;

        for path in sorted_paths {
            self.current_module = path.clone();
            if let Some(module) = self.registry.get(path) {
                let func_names: HashSet<String> = module
                    .program
                    .functions
                    .iter()
                    .map(|f| f.name.clone())
                    .collect();
                let global_names: HashSet<String> = module
                    .program
                    .globals
                    .iter()
                    .map(|g| g.name.clone())
                    .collect();
                let struct_names: HashSet<String> = module
                    .program
                    .structs
                    .iter()
                    .map(|s| s.name.clone())
                    .collect();
                let enum_names: HashSet<String> = module
                    .program
                    .enums
                    .iter()
                    .map(|e| e.name.clone())
                    .collect();

                let filtered = Program {
                    module_path: Some(path.clone()),
                    globals: merged
                        .globals
                        .iter()
                        .filter(|g| global_names.contains(&g.name))
                        .cloned()
                        .collect(),
                    functions: merged
                        .functions
                        .iter()
                        .filter(|f| func_names.contains(&f.name))
                        .cloned()
                        .collect(),
                    structs: merged
                        .structs
                        .iter()
                        .filter(|s| struct_names.contains(&s.name))
                        .cloned()
                        .collect(),
                    enums: merged
                        .enums
                        .iter()
                        .filter(|e| enum_names.contains(&e.name))
                        .cloned()
                        .collect(),
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

        self.current_module = saved_module;
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
        writeln!(out, "#ifndef {}", guard).unwrap();
        writeln!(out, "#define {}", guard).unwrap();
        out.push('\n');

        let seen_headers = collect_type_headers_and_decls(&self.inferencer, prog).0;
        for hdr in &seen_headers {
            writeln!(out, "#include {hdr}").unwrap();
        }
        out.push('\n');

        for import in &module.imports {
            if self.registry.contains(&import.path) {
                let dep = format!("{}.h", import.path.join("_"));
                writeln!(out, "#include \"{}\"", dep).unwrap();
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
            if global.is_public {
                let ty = match self.inferencer.store.resolve(global.inferred) {
                    Ok(t) => self.type_to_c_name_with_module(&t, &module.path),
                    Err(_) => global
                        .annotation
                        .as_ref()
                        .map(|a| a.to_c_repr().name)
                        .unwrap_or_else(|| "int".to_string()),
                };
                let gname = mangle_global(&module.path, &global.name);
                let const_ = if global.is_const { "const " } else { "" };
                writeln!(out, "extern {const_}{ty} {};", gname).unwrap();
            }
        }
        if prog.globals.iter().any(|g| g.is_public) {
            out.push('\n');
        }

        for func in &prog.functions {
            let ret = self.resolve_return_type_c_name(func);
            let fname = if func.name == "main" {
                "main".to_string()
            } else {
                mangle_function(&module.path, &func.name)
            };
            let params = self.format_function_params_with_module(&func.params, &module.path);
            writeln!(out, "{} {}({});", ret, fname, params).unwrap();
        }

        out.push('\n');
        writeln!(out, "#endif /* {} */", guard).unwrap();
        out
    }

    /// Generate a per-module C source file using the filtered (inferred) program data.
    fn generate_module_c_code_from_program(&self, prog: &Program, module: &Module) -> String {
        let mut out = String::new();
        let header = format!("{}.h", module.path.join("_"));
        writeln!(out, "#include \"{}\"", header).unwrap();

        for import in &module.imports {
            if self.registry.contains(&import.path) {
                let dep = format!("{}.h", import.path.join("_"));
                writeln!(out, "#include \"{}\"", dep).unwrap();
            }
        }
        for inc in &module.includes {
            writeln!(out, "#include \"{}\"", inc.path).unwrap();
        }
        out.push('\n');

        let (seen_headers, _) = collect_type_headers_and_decls(&self.inferencer, prog);
        for hdr in &seen_headers {
            writeln!(out, "#include {hdr}").unwrap();
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
