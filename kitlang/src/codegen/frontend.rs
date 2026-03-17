use crate::error::CompileResult;
use crate::{KitParser, Rule, error::CompilationError};
use pest::Parser;

use std::collections::HashSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::codegen::ast::{Block, Expr, Function, GlobalDecl, Include, Program, Stmt};
use crate::codegen::compiler::{CompilerMeta, CompilerOptions, Toolchain};
use crate::codegen::inference::TypeInferencer;
use crate::codegen::parser::Parser as CodeParser;
use crate::codegen::type_ast::{EnumDefinition, StructDefinition};
use crate::codegen::types::{ToCRepr, Type};

pub struct Compiler {
    files: Vec<PathBuf>,
    output: PathBuf,
    c_output: PathBuf,
    includes: Vec<Include>,
    libs: Vec<String>,
    parser: CodeParser,
    inferencer: TypeInferencer,
}

impl Compiler {
    pub fn new(files: Vec<PathBuf>, output: impl AsRef<Path>, libs: Vec<String>) -> Self {
        Self {
            files,
            output: output.as_ref().to_path_buf(),
            c_output: output.as_ref().with_extension("c"),
            includes: Vec::new(),
            libs,
            parser: CodeParser::new(),
            inferencer: TypeInferencer::new(),
        }
    }

    fn parse(&mut self) -> CompileResult<Program> {
        let mut includes = Vec::new();
        let mut globals = Vec::new();
        let mut functions = Vec::new();
        let mut structs = Vec::new();
        let mut enums = Vec::new();

        // Track file encoding (future-proofing)
        // true = UTF-8, false = binary (rejected)
        let mut files_utf8 = vec![true; self.files.len()];

        for (idx, file) in self.files.iter().enumerate() {
            let input = match std::fs::read_to_string(file) {
                Ok(content) => content,
                Err(err) => {
                    files_utf8[idx] = false;
                    return Err(CompilationError::Io(err));
                }
            };

            let pairs = KitParser::parse(Rule::program, &input)
                .map_err(|e| CompilationError::ParseError(e.to_string()))?;

            for pair in pairs {
                match pair.as_rule() {
                    Rule::include_stmt => {
                        includes.push(self.parser.parse_include(pair));
                    }

                    Rule::var_decl => {
                        globals.push(self.parser.parse_global_var_decl(pair)?);
                    }

                    Rule::function_decl => {
                        functions.push(self.parser.parse_function(pair)?);
                    }

                    Rule::type_def => {
                        for child in pair.into_inner() {
                            match child.as_rule() {
                                Rule::enum_def => {
                                    enums.push(self.parser.parse_enum_def(child)?);
                                    break;
                                }
                                Rule::struct_def => {
                                    structs.push(self.parser.parse_struct_def(child)?);
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }

                    _ => {}
                }
            }
        }

        self.includes.clone_from(&includes);

        Ok(Program {
            includes,
            imports: HashSet::new(),
            globals,
            functions,
            structs,
            enums,
        })
    }

    /// Generate C code from the AST and write it to the output path
    fn transpile_with_program(&mut self, prog: &Program) {
        let c_code = self.generate_c_code(prog);
        if let Err(e) = std::fs::write(&self.c_output, c_code) {
            panic!("Failed to write output: {e}");
        }
    }

    fn generate_c_code(&self, prog: &Program) -> String {
        let mut out = String::new();

        // emit regular includes from the source `include` statements
        for inc in &prog.includes {
            // Emit as #include "path" as per C preprocessor rules for Kit includes.
            let line = format!("#include \"{}\"\n", inc.path);
            out.push_str(&line);
        }

        let mut seen_headers = HashSet::new();
        // Vec preserves order
        let mut seen_declarations: Vec<String> = Vec::new();

        let mut collect_from_type = |t: &Type| {
            let ctype = t.to_c_repr();
            for h in ctype.headers {
                seen_headers.insert(h);
            }
            if let Some(d) = ctype.declaration
                && !seen_declarations.contains(&d)
            {
                seen_declarations.push(d);
            }
        };

        // Scan all types to gather required headers BEFORE emitting code that uses them
        // Scan struct field types
        for struct_def in &prog.structs {
            for field in &struct_def.fields {
                if let Ok(ty) = self.inferencer.store.resolve(field.ty) {
                    collect_from_type(&ty);
                } else if let Some(ann) = &field.annotation {
                    collect_from_type(ann);
                }
            }
        }

        // Scan enum variant argument types
        for enum_def in &prog.enums {
            for variant in &enum_def.variants {
                for arg in &variant.args {
                    if let Ok(ty) = self.inferencer.store.resolve(arg.ty) {
                        collect_from_type(&ty);
                    } else if let Some(ann) = &arg.annotation {
                        collect_from_type(ann);
                    }
                }
            }
        }

        // Scan global variable types
        for global in &prog.globals {
            if let Ok(ty) = self.inferencer.store.resolve(global.inferred) {
                collect_from_type(&ty);
            }
        }

        // scan every function signature & body for types to gather their headers/typedefs
        for func in &prog.functions {
            // Use inferred return type
            if let Some(ret_id) = func.inferred_return {
                if let Ok(ty) = self.inferencer.store.resolve(ret_id) {
                    collect_from_type(&ty);
                }
            } else if let Some(r) = &func.return_type {
                collect_from_type(r);
            }

            for p in &func.params {
                // Use inferred param type
                if let Ok(ty) = self.inferencer.store.resolve(p.ty) {
                    collect_from_type(&ty);
                } else if let Some(ann) = &p.annotation {
                    collect_from_type(ann);
                }
            }

            for stmt in &func.body.stmts {
                if let Stmt::VarDecl { inferred, .. } = stmt
                    && let Ok(ty) = self.inferencer.store.resolve(*inferred)
                {
                    collect_from_type(&ty);
                }
            }
        }

        // emit unique headers
        for hdr in seen_headers {
            writeln!(out, "#include {hdr}").unwrap();
        }
        out.push('\n');

        // emit each unique typedef
        for decl in seen_declarations {
            out.push_str(&decl);
            out.push('\n');
        }

        // Emit struct declarations
        for struct_def in &prog.structs {
            out.push_str(&self.generate_struct_declaration(struct_def, &prog.structs));
            out.push('\n');
        }

        // Emit enum declarations
        for enum_def in &prog.enums {
            out.push_str(&self.generate_enum_declaration(enum_def));
            out.push('\n');
        }

        // Emit global variable declarations
        for global in &prog.globals {
            out.push_str(&self.transpile_global(global));
            out.push('\n');
        }

        // emit functions as before...
        for func in &prog.functions {
            out.push_str(&self.transpile_function(func));
            out.push_str("\n\n");
        }
        out
    }

    fn transpile_global(&self, global: &GlobalDecl) -> String {
        let ty = self.inferencer.store.resolve(global.inferred).map_or_else(
            |_| "int".to_string(),
            |t| {
                if let Type::Named(name) = &t {
                    if self.inferencer.is_struct_type(name) {
                        format!("struct {}", name)
                    } else {
                        t.to_c_repr().name
                    }
                } else {
                    t.to_c_repr().name
                }
            },
        );

        let const_prefix = if global.is_const { "const " } else { "" };

        match &global.init {
            Some(expr) => {
                let init_str = self.transpile_expr(expr);
                format!("{const_prefix}{ty} {} = {init_str};", global.name)
            }
            None => {
                format!("{const_prefix}{ty} {};", global.name)
            }
        }
    }

    fn generate_struct_declaration(
        &self,
        struct_def: &StructDefinition,
        all_structs: &[StructDefinition],
    ) -> String {
        let field_decls: Vec<String> = struct_def
            .fields
            .iter()
            .map(|field| {
                // Resolve field type
                let ty = self
                    .inferencer
                    .store
                    .resolve(field.ty)
                    .ok()
                    .or(field.annotation.as_ref().cloned())
                    .unwrap_or(Type::Void);

                let c_repr = ty.to_c_repr();

                // Apply const modifier if present
                let prefix = if field.is_const { "const " } else { "" };

                // Get type name, handling struct references correctly
                let type_name = if let Type::Named(name) = &ty {
                    // Check if this named type is actually a struct definition
                    let is_struct = all_structs.iter().any(|s| s.name == *name);
                    if is_struct {
                        format!("struct {}", name)
                    } else {
                        c_repr.name.clone()
                    }
                } else {
                    c_repr.name.clone()
                };

                format!("    {}{} {};", prefix, type_name, field.name)
            })
            .collect();

        format!(
            "struct {} {{\n{}\n}};",
            struct_def.name,
            field_decls.join("\n")
        )
    }

    /// Lowers a Kit enum definition into its C representation.
    ///
    /// Simple enums (variants without associated data) are emitted as plain C `enum`s.
    /// Enums with data-carrying variants are compiled into a tagged-union layout:
    /// - a discriminant `enum` to track the active variant,
    /// - one `struct` per data-carrying variant,
    /// - a top-level `struct` containing the discriminant and a `union` of variant data.
    ///
    /// For variants with fields, constructor functions are generated to initialize the
    /// correct discriminant and populate the union safely, avoiding error-prone manual
    /// initialization in C.
    fn generate_enum_declaration(&self, enum_def: &EnumDefinition) -> String {
        let mut output = String::new();

        // Check if all variants are simple (no arguments)
        let all_simple = enum_def.variants.iter().all(|v| v.args.is_empty());

        if all_simple {
            // Simple enum: generate C enum
            let variants: Vec<String> = enum_def
                .variants
                .iter()
                .map(|v| format!("    {}_{}", enum_def.name, v.name))
                .collect();

            output.push_str(&format!(
                "typedef enum {{\n{}\n}} {};\n\n",
                variants.join(",\n"),
                enum_def.name
            ));
        } else {
            // Complex enum: generate C enum for discriminant
            let discriminant_variants: Vec<String> = enum_def
                .variants
                .iter()
                .map(|v| format!("    {}_{}", enum_def.name, v.name))
                .collect();

            output.push_str(&format!(
                "typedef enum {{\n{}\n}} {}_Discriminant;\n\n",
                discriminant_variants.join(",\n"),
                enum_def.name
            ));

            // Generate variant data structs
            for v in enum_def.variants.iter().filter(|v| !v.args.is_empty()) {
                let field_decls: Vec<String> = v
                    .args
                    .iter()
                    .map(|arg| {
                        let ty = self
                            .inferencer
                            .store
                            .resolve(arg.ty)
                            .ok()
                            .or(arg.annotation.as_ref().cloned())
                            .unwrap_or(Type::Void);
                        let c_repr = ty.to_c_repr();
                        format!("    {} {};", c_repr.name, arg.name)
                    })
                    .collect();

                output.push_str(&format!(
                    "typedef struct {{\n{}\n}} {}_{}_data;\n\n",
                    field_decls.join("\n"),
                    enum_def.name,
                    v.name
                ));
            }

            // Generate union of variant data
            let union_fields: Vec<String> = enum_def
                .variants
                .iter()
                .filter(|v| !v.args.is_empty())
                .map(|v| {
                    format!(
                        "    {}_{}_data {};",
                        enum_def.name,
                        v.name,
                        v.name.to_lowercase()
                    )
                })
                .collect();

            let struct_body = format!(
                "    {}_Discriminant _discriminant;\n    union {{\n{}\n    }} _variant;",
                enum_def.name,
                union_fields.join("\n")
            );

            output.push_str(&format!(
                "typedef struct {{\n{}\n}} {};\n\n",
                struct_body, enum_def.name
            ));
        }

        // Generate constructor functions for variants with arguments
        for v in enum_def.variants.iter().filter(|v| !v.args.is_empty()) {
            let params: Vec<String> = v
                .args
                .iter()
                .map(|arg| {
                    let ty = self
                        .inferencer
                        .store
                        .resolve(arg.ty)
                        .ok()
                        .or(arg.annotation.as_ref().cloned())
                        .unwrap_or(Type::Void);
                    let c_repr = ty.to_c_repr();
                    format!("{} {}", c_repr.name, arg.name)
                })
                .collect();

            let arg_names: Vec<String> = v.args.iter().map(|arg| arg.name.clone()).collect();

            let assignments: Vec<String> = v
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

            output.push_str(&format!(
                "{} {}_{}_new({}) {{\n    {} result;\n    result._discriminant = {}_{};\n{}\n    return result;\n}}\n\n",
                enum_def.name,
                enum_def.name,
                v.name,
                params.join(", "),
                enum_def.name,
                enum_def.name,
                v.name,
                assignments.join("\n")
            ));
        }

        output
    }

    fn transpile_function(&self, func: &Function) -> String {
        let return_type = if func.name == "main" {
            "int".to_string()
        } else {
            // Try inferred return type first
            func.inferred_return
                .and_then(|id| self.inferencer.store.resolve(id).ok())
                .map(|t| t.to_c_repr().name)
                .or_else(|| func.return_type.as_ref().map(|t| t.to_c_repr().name))
                .unwrap_or_else(|| "void".to_string())
        };

        let params = func
            .params
            .iter()
            .map(|p| {
                let ty_name = self
                    .inferencer
                    .store
                    .resolve(p.ty)
                    .map(|t| t.to_c_repr().name)
                    .or_else(|_| p.annotation.as_ref().map(|t| t.to_c_repr().name).ok_or(()))
                    .unwrap_or("void*".to_string()); // Fallback
                format!("{} {}", ty_name, p.name)
            })
            .collect::<Vec<_>>()
            .join(", ");

        let mut body_code = self.transpile_block(&func.body);

        if func.name == "main" {
            let has_return = func
                .body
                .stmts
                .iter()
                .any(|stmt| matches!(stmt, Stmt::Return(_)));
            if !has_return {
                // Insert return 0 before the closing brace
                if let Some(pos) = body_code.rfind('}') {
                    body_code.insert_str(pos, "return 0;\n");
                }
            }
        }

        format!("{} {}({}) {}", return_type, func.name, params, body_code)
    }

    fn transpile_block(&self, block: &Block) -> String {
        let mut code = String::from("{\n");
        for stmt in &block.stmts {
            let stmt_code = match stmt {
                Stmt::VarDecl {
                    name,
                    annotation: _,
                    inferred,
                    init,
                } => {
                    let ty_str = self.inferencer.store.resolve(*inferred).map_or_else(
                        |_| "auto".to_string(),
                        |t| {
                            // For Named types that are actually structs, use "struct <name>"
                            if let Type::Named(name) = &t {
                                // Check if this named type is a struct
                                if self.inferencer.is_struct_type(name) {
                                    format!("struct {}", name)
                                } else {
                                    t.to_c_repr().name
                                }
                            } else {
                                t.to_c_repr().name
                            }
                        },
                    );

                    match init {
                        Some(expr) => {
                            let init_str = self.transpile_expr(expr);
                            format!("{ty_str} {name} = {init_str};\n")
                        }
                        None => {
                            format!("{ty_str} {name};\n")
                        }
                    }
                }
                Stmt::Expr(expr) => {
                    format!("{};\n", self.transpile_expr(expr))
                }
                Stmt::Return(expr) => {
                    if let Some(e) = expr {
                        format!("return {};\n", self.transpile_expr(e))
                    } else {
                        "return;\n".to_string()
                    }
                }
                Stmt::If {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    let mut s = format!("if ({}) ", self.transpile_expr(cond));
                    s.push_str(&self.transpile_block(then_branch));
                    if let Some(else_b) = else_branch {
                        s.push_str(" else ");
                        s.push_str(&self.transpile_block(else_b));
                    }
                    s.push('\n');
                    s
                }
                Stmt::While { cond, body } => {
                    let mut s = format!("while ({}) ", self.transpile_expr(cond));
                    s.push_str(&self.transpile_block(body));
                    s.push('\n');
                    s
                }
                Stmt::For { var, iter, body } => {
                    let mut s = if let Expr::RangeLiteral { start, end } = iter {
                        let start_str = self.transpile_expr(start);
                        let end_str = self.transpile_expr(end);
                        format!("for (int {var} = {start_str}; {var} < {end_str}; ++{var}) ")
                    } else {
                        let iter_str = self.transpile_expr(iter);
                        format!("for (int {var} = 0; {var} < {iter_str}; ++{var}) ")
                    };
                    s.push_str(&self.transpile_block(body));
                    s
                }
                Stmt::Break => "break;\n".to_string(),
                Stmt::Continue => "continue;\n".to_string(),
            };

            for line in stmt_code.lines() {
                code.push_str("    ");
                code.push_str(line);
                code.push('\n');
            }
        }
        code.push('}');
        code
    }

    fn transpile_expr(&self, expr: &Expr) -> String {
        match expr {
            Expr::Identifier(name, _) => name.clone(),
            Expr::Literal(lit, _) => lit.to_c(),
            Expr::Call {
                callee,
                args,
                ty: _,
            } => {
                // Check if this is an enum variant constructor call (by simple name)
                if let Some(variant_info) = self
                    .inferencer
                    .symbols()
                    .lookup_enum_variant_by_simple_name(callee)
                {
                    let args_str = args
                        .iter()
                        .map(|a| self.transpile_expr(a))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "{}_{}_new({})",
                        variant_info.enum_name, variant_info.variant_name, args_str
                    )
                } else {
                    let args_str = args
                        .iter()
                        .map(|a| self.transpile_expr(a))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{callee}({args_str})")
                }
            }
            Expr::UnaryOp { op, expr, ty: _ } => {
                let expr_str = self.transpile_expr(expr);
                format!("{}({})", op.to_c_str(), expr_str)
            }
            Expr::BinaryOp {
                op,
                left,
                right,
                ty: _,
            } => {
                let left_str = self.transpile_expr(left);
                let right_str = self.transpile_expr(right);
                format!("({left_str} {} {right_str})", op.to_c_str())
            }
            Expr::Assign {
                op,
                left,
                right,
                ty: _,
            } => {
                let left_str = self.transpile_expr(left);
                let right_str = self.transpile_expr(right);
                format!("{left_str} {} {right_str}", op.to_c_str())
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
                ty: _,
            } => {
                let cond_str = self.transpile_expr(cond);
                let then_str = self.transpile_expr(then_branch);
                let else_str = self.transpile_expr(else_branch);
                format!("({cond_str} ? {then_str} : {else_str})")
            }
            Expr::RangeLiteral { .. } => {
                // Should technically not be used alone, but return something safe to avoid panic
                "/* range literal */ 0".to_string()
            }
            Expr::StructInit {
                ty,
                struct_type: _,
                fields,
            } => {
                // Resolve struct type to get name
                let struct_name = match self.inferencer.store.resolve(*ty) {
                    Ok(t) => {
                        if let Type::Struct { name, .. } = t {
                            name
                        } else if let Type::Named(name) = t {
                            // For Named types that are structs, use the name
                            name
                        } else {
                            "UNKNOWN_STRUCT".to_string()
                        }
                    }
                    Err(e) => {
                        eprintln!("Warning: Failed to resolve struct type: {}", e);
                        "UNKNOWN_STRUCT".to_string()
                    }
                };

                // Generate field initializers using C99 designated initializers
                let field_inits: Vec<String> = fields
                    .iter()
                    .map(|f| {
                        let value = self.transpile_expr(&f.value);
                        format!(".{} = {}", f.name, value)
                    })
                    .collect();

                format!("(struct {}){{{}}}", struct_name, field_inits.join(", "))
            }
            Expr::FieldAccess {
                expr,
                field_name,
                ty: _,
            } => {
                let expr_str = self.transpile_expr(expr);
                format!("{}.{}", expr_str, field_name)
            }
            Expr::EnumVariant {
                enum_name,
                variant_name,
                ty: _,
            } => {
                // Simple enum variant - check if it's a simple or complex enum
                let enum_def = self.inferencer.symbols().lookup_enum(enum_name);
                let is_simple = enum_def
                    .map(|e| e.variants.iter().all(|v| v.args.is_empty()))
                    .unwrap_or(false);

                if is_simple {
                    // Simple enum: just use the discriminant constant
                    format!("{}_{}", enum_name, variant_name)
                } else {
                    // Complex enum: need full struct initialization
                    format!(
                        "{{.{} = {}_{}, ._variant = {{0}}}}",
                        "_discriminant", enum_name, variant_name
                    )
                }
            }
            Expr::EnumInit {
                enum_name,
                variant_name,
                args,
                ty: _,
            } => {
                // Check if this is a simple variant (no args)
                if args.is_empty() {
                    // Simple variant - need to create a full struct initialization for complex enums
                    // For simple enums: just use the discriminant constant
                    let enum_def = self.inferencer.symbols().lookup_enum(enum_name);
                    let is_simple = enum_def
                        .map(|e| e.variants.iter().all(|v| v.args.is_empty()))
                        .unwrap_or(false);

                    if is_simple {
                        format!("{}_{}", enum_name, variant_name)
                    } else {
                        // Complex enum: initialize the full struct with designated initializers
                        format!(
                            "{{.{} = {}_{}, ._variant = {{0}}}}",
                            "_discriminant", enum_name, variant_name
                        )
                    }
                } else {
                    // Complex variant - call the constructor with defaults inlined
                    let enum_def = self.inferencer.symbols().lookup_enum(enum_name);
                    let variant_def =
                        enum_def.and_then(|e| e.variants.iter().find(|v| v.name == *variant_name));

                    let args_str = if let Some(variant) = variant_def {
                        let mut full_args = args.clone();
                        for i in args.len()..variant.args.len() {
                            if let Some(default) = &variant.args[i].default {
                                full_args.push(default.clone());
                            }
                        }
                        full_args
                            .iter()
                            .map(|a| self.transpile_expr(a))
                            .collect::<Vec<_>>()
                            .join(", ")
                    } else {
                        args.iter()
                            .map(|a| self.transpile_expr(a))
                            .collect::<Vec<_>>()
                            .join(", ")
                    };

                    format!("{}_{}_new({})", enum_name, variant_name, args_str)
                }
            }
        }
    }

    pub fn compile(&mut self) -> CompileResult<()> {
        let mut prog = self.parse()?;

        self.inferencer.infer_program(&mut prog)?;
        self.transpile_with_program(&prog);

        let detected = Toolchain::executable_path().ok_or(CompilationError::ToolchainNotFound)?;

        // TODO: Handle non-UTF-8 paths
        let target_path = self
            .output
            .clone()
            .into_os_string()
            .into_string()
            .map_err(|_| CompilationError::InvalidOutputPath)?;

        let opts = CompilerOptions::new(CompilerMeta(detected.0))
            .link_libs(&self.libs)
            .lib_paths(&["/usr/local/lib"])
            .sources(&[&self.c_output])
            .output(&target_path)
            .build();

        let mut cmd = Command::new(&detected.1);

        // Get C99 compiler flags from the toolchain to make sure correct C standard based on
        // toolchain and include paths are used
        let compiler_flags = detected.0.get_compiler_flags();
        cmd.args(&compiler_flags);
        cmd.arg(&self.c_output);

        match detected.0 {
            Toolchain::Gcc | Toolchain::Clang => {
                cmd.arg("-o").arg(&self.output);
            }
            #[cfg(windows)]
            Toolchain::Msvc => {
                cmd.arg(format!("/Fe:{}", self.output.display()));
            }
            Toolchain::Other => {
                return Err(CompilationError::UnsupportedToolchain(
                    detected.1.display().to_string(),
                ));
            }
        }

        cmd.args(&opts.link_opts);

        let output = cmd.output().map_err(CompilationError::Io)?;
        let status = output.status;

        if !status.success() {
            // Keep the C source file for debugging.
            return Err(CompilationError::CCompileError(output.stderr));
        }

        // If we don't want to keep C source files, delete after compilation
        if std::env::var("KEEP_C").is_err()
            && let Err(err) = std::fs::remove_file(&self.c_output)
        {
            log::warn!(
                "Failed to remove intermediate C file {}: {err}",
                self.c_output.display()
            );
        }

        Ok(())
    }
}
