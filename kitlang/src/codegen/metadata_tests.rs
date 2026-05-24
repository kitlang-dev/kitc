use super::ast::{
    Attributed, Block, Function, GlobalDecl, Literal, MetaArg, Metadata, has_meta, has_no_mangle,
};
use super::frontend::Compiler;
use super::module::{Module, ModulePath, ModuleRegistry};
use super::type_ast::{EnumDefinition, EnumVariant, StructDefinition};
use super::types::TypeId;
use crate::Rule;

#[test]
fn test_metadata_has_name() {
    let m = Metadata {
        name: "extern".to_string(),
        args: vec![],
    };
    assert!(m.has_name("extern"));
    assert!(!m.has_name("expose"));
}

#[test]
fn test_metadata_has_meta() {
    let metas = vec![
        Metadata {
            name: "inline".to_string(),
            args: vec![],
        },
        Metadata {
            name: "extern".to_string(),
            args: vec![],
        },
    ];
    assert!(has_meta(&metas, "extern"));
    assert!(has_meta(&metas, "inline"));
    assert!(!has_meta(&metas, "noreturn"));
}

#[test]
fn test_metadata_has_no_mangle() {
    let no_mangle = vec![Metadata {
        name: "extern".to_string(),
        args: vec![],
    }];
    assert!(has_no_mangle(&no_mangle));

    let expose = vec![Metadata {
        name: "expose".to_string(),
        args: vec![],
    }];
    assert!(has_no_mangle(&expose));

    let other = vec![Metadata {
        name: "inline".to_string(),
        args: vec![],
    }];
    assert!(!has_no_mangle(&other));
}

#[test]
fn test_metadata_meta_arg_identifier() {
    let m = Metadata {
        name: "meta".to_string(),
        args: vec![MetaArg::Identifier("MyType".to_string())],
    };
    assert_eq!(m.name, "meta");
    assert_eq!(m.args.len(), 1);
    match &m.args[0] {
        MetaArg::Identifier(s) => assert_eq!(s, "MyType"),
        _ => panic!("expected identifier"),
    }
}

#[test]
fn test_metadata_meta_arg_literal() {
    let m = Metadata {
        name: "meta".to_string(),
        args: vec![MetaArg::Literal(Literal::Int(42))],
    };
    assert_eq!(m.args.len(), 1);
    match &m.args[0] {
        MetaArg::Literal(lit) => assert_eq!(*lit, Literal::Int(42)),
        _ => panic!("expected literal"),
    }
}

#[test]
fn test_function_has_no_mangle_extern() {
    let f = Function {
        name: "foo".to_string(),
        params: vec![],
        return_type: None,
        inferred_return: None,
        body: Block { stmts: vec![] },
        is_public: true,
        metadata: vec![Metadata {
            name: "extern".to_string(),
            args: vec![],
        }],
    };
    assert!(f.has_no_mangle());
    assert!(f.is_extern());
}

#[test]
fn test_function_has_no_mangle_expose() {
    let f = Function {
        name: "foo".to_string(),
        params: vec![],
        return_type: None,
        inferred_return: None,
        body: Block { stmts: vec![] },
        is_public: true,
        metadata: vec![Metadata {
            name: "expose".to_string(),
            args: vec![],
        }],
    };
    assert!(f.has_no_mangle());
    assert!(!f.is_extern());
}

#[test]
fn test_function_no_metadata() {
    let f = Function {
        name: "foo".to_string(),
        params: vec![],
        return_type: None,
        inferred_return: None,
        body: Block { stmts: vec![] },
        is_public: true,
        metadata: vec![],
    };
    assert!(!f.has_no_mangle());
    assert!(!f.is_extern());
}

#[test]
fn test_global_decl_has_no_mangle() {
    let g = GlobalDecl {
        name: "GLOBAL".to_string(),
        annotation: None,
        inferred: TypeId::default(),
        init: None,
        is_const: false,
        is_public: true,
        metadata: vec![Metadata {
            name: "extern".to_string(),
            args: vec![],
        }],
    };
    assert!(g.has_no_mangle());
    assert!(g.is_extern());
}

#[test]
fn test_check_extern_name_duplicate() {
    let mut registry = ModuleRegistry::new();
    assert!(registry.register_extern_name("myFunc").is_ok());
    assert!(registry.register_extern_name("myFunc").is_err());
}

#[test]
fn test_check_extern_name_distinct() {
    let mut registry = ModuleRegistry::new();
    assert!(registry.register_extern_name("funcA").is_ok());
    assert!(registry.register_extern_name("funcB").is_ok());
}

#[test]
fn test_extern_name_before_registration() {
    let registry = ModuleRegistry::new();
    // check_extern_name for a name that doesn't exist should succeed
    assert!(registry.check_extern_name("newFunc").is_ok());
}

#[test]
fn test_parse_extern_function() {
    use super::parser::Parser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "#[extern] function externFunc() { printf(\"hello\"); }\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    let mut functions = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::function_decl {
            functions.push(parser.parse_function(pair).unwrap());
        }
    }
    assert_eq!(functions.len(), 1);
    let f = &functions[0];
    assert_eq!(f.name, "externFunc");
    assert!(f.has_no_mangle());
    assert!(f.is_extern());
}

#[test]
fn test_parse_expose_function() {
    use super::parser::Parser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "#[expose] function exposedFunc() { }\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    let mut functions = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::function_decl {
            functions.push(parser.parse_function(pair).unwrap());
        }
    }
    assert_eq!(functions.len(), 1);
    let f = &functions[0];
    assert_eq!(f.name, "exposedFunc");
    assert!(f.has_no_mangle());
    assert!(!f.is_extern());
}

#[test]
fn test_parse_private_function() {
    use super::parser::Parser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "private function helper() { }\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    let mut functions = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::function_decl {
            functions.push(parser.parse_function(pair).unwrap());
        }
    }
    assert_eq!(functions.len(), 1);
    let f = &functions[0];
    assert_eq!(f.name, "helper");
    assert!(!f.is_public);
}

#[test]
fn test_parse_public_function() {
    use super::parser::Parser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "public function pubFunc() { }\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    let mut functions = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::function_decl {
            functions.push(parser.parse_function(pair).unwrap());
        }
    }
    assert_eq!(functions.len(), 1);
    let f = &functions[0];
    assert_eq!(f.name, "pubFunc");
    assert!(f.is_public);
}

#[test]
fn test_parse_function_default_public() {
    use super::parser::Parser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "function defaultPublic() { }\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    let mut functions = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::function_decl {
            functions.push(parser.parse_function(pair).unwrap());
        }
    }
    assert_eq!(functions.len(), 1);
    let f = &functions[0];
    assert!(f.is_public);
    assert!(!f.has_no_mangle());
}

#[test]
fn test_parse_extern_global() {
    use super::parser::Parser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "#[extern] var globalExtern: int = 42;\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    let mut globals = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::var_decl {
            globals.push(parser.parse_global_var_decl(pair).unwrap());
        }
    }
    assert_eq!(globals.len(), 1);
    let g = &globals[0];
    assert_eq!(g.name, "globalExtern");
    assert!(g.has_no_mangle());
    assert!(g.is_extern());
}

#[test]
fn test_parse_expose_global() {
    use super::parser::Parser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "#[expose] var globalExpose: int = 7;\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    let mut globals = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::var_decl {
            globals.push(parser.parse_global_var_decl(pair).unwrap());
        }
    }
    assert_eq!(globals.len(), 1);
    let g = &globals[0];
    assert_eq!(g.name, "globalExpose");
    assert!(g.has_no_mangle());
    assert!(!g.is_extern());
}

#[test]
fn test_parse_private_global() {
    use super::parser::Parser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "private var privGlobal: int = 0;\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    let mut globals = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::var_decl {
            globals.push(parser.parse_global_var_decl(pair).unwrap());
        }
    }
    assert_eq!(globals.len(), 1);
    let g = &globals[0];
    assert_eq!(g.name, "privGlobal");
    assert!(!g.is_public);
}

#[test]
fn test_parse_extern_with_args() {
    use super::parser::Parser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "#[extern(\"C\")] function externWithArg() { }\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    let mut functions = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::function_decl {
            functions.push(parser.parse_function(pair).unwrap());
        }
    }
    assert_eq!(functions.len(), 1);
    let f = &functions[0];
    assert_eq!(f.name, "externWithArg");
    assert!(f.has_no_mangle());
    assert!(f.is_extern());
    assert_eq!(
        f.metadata.len(),
        1,
        "expected 1 metadata, got {}: {:?}",
        f.metadata.len(),
        f.metadata
    );
    assert_eq!(f.metadata[0].name, "extern");
    assert_eq!(
        f.metadata[0].args.len(),
        1,
        "expected 1 arg, got {}: {:?}",
        f.metadata[0].args.len(),
        f.metadata[0].args
    );
    match &f.metadata[0].args[0] {
        MetaArg::Literal(Literal::String(s)) => assert_eq!(s, "C"),
        other => panic!("expected string literal \"C\", got {other:?}"),
    }
}

#[test]
fn test_cross_module_extern_collision() {
    use super::module::Module;
    use crate::codegen::ast::Program;
    use std::path::PathBuf;

    let mut registry = ModuleRegistry::new();
    let mod_a = ModulePath::from_parts(&["mod_a"]);
    let mod_b = ModulePath::from_parts(&["mod_b"]);

    let extern_meta = vec![Metadata {
        name: "extern".to_string(),
        args: vec![],
    }];

    fn make_module(path: ModulePath, func_name: &str, meta: Vec<Metadata>) -> Module {
        let mut program = Program::empty();
        program.module_path = Some(path.clone());
        program.functions = vec![Function {
            name: func_name.to_string(),
            params: vec![],
            return_type: None,
            inferred_return: None,
            body: Block { stmts: vec![] },
            is_public: true,
            metadata: meta,
        }];
        Module::new(
            path,
            PathBuf::from(format!("{}.kit", func_name)),
            vec![],
            vec![],
            program,
        )
    }

    // First module with #[extern] foo should succeed
    let first = make_module(mod_a, "foo", extern_meta.clone());
    assert!(registry.register(first).is_ok());

    // Second module with #[extern] foo should fail - duplicate extern name
    let second = make_module(mod_b, "foo", extern_meta);
    let err = registry.register(second).unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("foo") || err_str.contains("duplicate") || err_str.contains("Duplicate"),
        "expected DuplicateSymbol error, got: {err_str}"
    );
}

#[test]
fn test_extern_c_output_contains_extern_prefix() {
    use std::io::Write;

    let dir = std::env::temp_dir().join("kit_test_extern_output");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let kit_path = dir.join("test_extern.kit");
    let mut file = std::fs::File::create(&kit_path).unwrap();
    writeln!(file, "include \"stdio.h\";").unwrap();
    writeln!(file, "#[extern] function externFunc() {{ printf(\"e\"); }}").unwrap();
    writeln!(file, "#[expose] function exposeFunc() {{ printf(\"x\"); }}").unwrap();
    writeln!(file, "function plainFunc() {{ printf(\"p\"); }}").unwrap();
    writeln!(file, "#[extern] var externGlobal: Int = 1;").unwrap();
    writeln!(file, "#[expose] var exposeGlobal: Int = 2;").unwrap();
    writeln!(file, "var plainGlobal: Int = 3;").unwrap();
    writeln!(
        file,
        "function main() {{ externFunc(); exposeFunc(); plainFunc(); return 0; }}"
    )
    .unwrap();
    drop(file);

    let mut compiler = Compiler::new(vec![kit_path], dir.join("test_extern"), vec![], vec![]);

    // SAFETY: `set_var`/`remove_var` are unsafe in Rust 2024 due to data-race risk,
    // but tests run single-threaded by default, so this is fine.
    // KEEP_C=1 prevents the compiler from deleting the generated .c file we need to inspect.
    unsafe { std::env::set_var("KEEP_C", "1") };
    compiler.compile().unwrap();
    // SAFETY: Remove the env var so it doesn't leak to other tests.
    unsafe { std::env::remove_var("KEEP_C") };

    let c_path = dir.join("test_extern_modules").join("test_extern.c");
    let c_code = std::fs::read_to_string(&c_path).unwrap_or_else(|e| {
        panic!("cannot read {}: {e}", c_path.display());
    });

    assert!(
        c_code.contains("extern void externFunc"),
        "extern function should have extern prefix\n--- C code:\n{c_code}"
    );
    assert!(
        !c_code.contains("extern void exposeFunc"),
        "expose function should NOT have extern prefix\n--- C code:\n{c_code}"
    );
    assert!(
        !c_code.contains("extern void plainFunc"),
        "plain function should NOT have extern prefix\n--- C code:\n{c_code}"
    );

    // Global checks
    assert!(
        c_code.contains("extern int externGlobal"),
        "extern global should have extern prefix\n--- C code:\n{c_code}"
    );
    assert!(
        !c_code.contains("extern int exposeGlobal"),
        "expose global should NOT have extern prefix\n--- C code:\n{c_code}"
    );
    assert!(
        !c_code.contains("extern int plainGlobal"),
        "plain global should NOT have extern prefix\n--- C code:\n{c_code}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_struct_has_no_mangle_extern() {
    let s = StructDefinition {
        name: "Foo".to_string(),
        fields: vec![],
        is_public: true,
        metadata: vec![Metadata {
            name: "extern".to_string(),
            args: vec![],
        }],
    };
    assert!(s.has_no_mangle());
    assert!(s.is_extern());
}

#[test]
fn test_struct_has_no_mangle_expose() {
    let s = StructDefinition {
        name: "Foo".to_string(),
        fields: vec![],
        is_public: true,
        metadata: vec![Metadata {
            name: "expose".to_string(),
            args: vec![],
        }],
    };
    assert!(s.has_no_mangle());
    assert!(!s.is_extern());
}

#[test]
fn test_struct_no_metadata() {
    let s = StructDefinition {
        name: "Foo".to_string(),
        fields: vec![],
        is_public: true,
        metadata: vec![],
    };
    assert!(!s.has_no_mangle());
    assert!(!s.is_extern());
}

#[test]
fn test_enum_has_no_mangle_extern() {
    let e = EnumDefinition {
        name: "Foo".to_string(),
        variants: vec![],
        is_public: true,
        metadata: vec![Metadata {
            name: "extern".to_string(),
            args: vec![],
        }],
    };
    assert!(e.has_no_mangle());
    assert!(e.is_extern());
}

#[test]
fn test_enum_variant_has_no_mangle_extern() {
    let v = EnumVariant {
        name: "Bar".to_string(),
        parent: "Foo".to_string(),
        args: vec![],
        default: None,
        metadata: vec![Metadata {
            name: "extern".to_string(),
            args: vec![],
        }],
    };
    assert!(v.has_no_mangle());
    assert!(v.is_extern());
}

#[test]
fn test_enum_variant_has_no_mangle_expose() {
    let v = EnumVariant {
        name: "Bar".to_string(),
        parent: "Foo".to_string(),
        args: vec![],
        default: None,
        metadata: vec![Metadata {
            name: "expose".to_string(),
            args: vec![],
        }],
    };
    assert!(v.has_no_mangle());
    assert!(!v.is_extern());
}

#[test]
fn test_enum_variant_no_metadata() {
    let v = EnumVariant {
        name: "Bar".to_string(),
        parent: "Foo".to_string(),
        args: vec![],
        default: None,
        metadata: vec![],
    };
    assert!(!v.has_no_mangle());
    assert!(!v.is_extern());
}

#[test]
fn test_parse_extern_struct() {
    use super::parser::Parser as CodeParser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "#[extern] struct ExternStruct { var x: int; }\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = CodeParser::new();
    let mut structs = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::type_def {
            let mut inner = pair.into_inner();
            let (metadata, is_public) = CodeParser::parse_metadata_and_modifiers(inner.next());
            for child in inner {
                if child.as_rule() == Rule::struct_def {
                    structs.push(
                        parser
                            .parse_struct_def(child, metadata.clone(), is_public)
                            .unwrap(),
                    );
                }
            }
        }
    }
    assert_eq!(structs.len(), 1);
    let s = &structs[0];
    assert_eq!(s.name, "ExternStruct");
    assert!(s.has_no_mangle());
    assert!(s.is_extern());
}

#[test]
fn test_parse_expose_struct() {
    use super::parser::Parser as CodeParser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "#[expose] struct ExposeStruct { var x: int; }\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = CodeParser::new();
    let mut structs = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::type_def {
            let mut inner = pair.into_inner();
            let (metadata, is_public) = CodeParser::parse_metadata_and_modifiers(inner.next());
            for child in inner {
                if child.as_rule() == Rule::struct_def {
                    structs.push(
                        parser
                            .parse_struct_def(child, metadata.clone(), is_public)
                            .unwrap(),
                    );
                }
            }
        }
    }
    assert_eq!(structs.len(), 1);
    let s = &structs[0];
    assert_eq!(s.name, "ExposeStruct");
    assert!(s.has_no_mangle());
    assert!(!s.is_extern());
}

#[test]
fn test_parse_extern_enum() {
    use super::parser::Parser as CodeParser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "#[extern] enum ExternEnum { A; B; }\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = CodeParser::new();
    let mut enums = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::type_def {
            let mut inner = pair.into_inner();
            let (metadata, is_public) = CodeParser::parse_metadata_and_modifiers(inner.next());
            for child in inner {
                if child.as_rule() == Rule::enum_def {
                    enums.push(
                        parser
                            .parse_enum_def(child, metadata.clone(), is_public)
                            .unwrap(),
                    );
                }
            }
        }
    }
    assert_eq!(enums.len(), 1);
    let e = &enums[0];
    assert_eq!(e.name, "ExternEnum");
    assert!(e.has_no_mangle());
    assert!(e.is_extern());
}

#[test]
fn test_parse_expose_enum() {
    use super::parser::Parser as CodeParser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "#[expose] enum ExposeEnum { A; B; }\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = CodeParser::new();
    let mut enums = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::type_def {
            let mut inner = pair.into_inner();
            let (metadata, is_public) = CodeParser::parse_metadata_and_modifiers(inner.next());
            for child in inner {
                if child.as_rule() == Rule::enum_def {
                    enums.push(
                        parser
                            .parse_enum_def(child, metadata.clone(), is_public)
                            .unwrap(),
                    );
                }
            }
        }
    }
    assert_eq!(enums.len(), 1);
    let e = &enums[0];
    assert_eq!(e.name, "ExposeEnum");
    assert!(e.has_no_mangle());
    assert!(!e.is_extern());
}

#[test]
fn test_parse_extern_enum_variant() {
    use super::parser::Parser as CodeParser;
    use crate::KitParser;
    use pest::Parser as PestParser;

    let source = "enum Foo { #[extern] Bar(x: int); }\n";
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = CodeParser::new();
    let mut enums = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::type_def {
            let mut inner = pair.into_inner();
            let (metadata, is_public) = CodeParser::parse_metadata_and_modifiers(inner.next());
            for child in inner {
                if child.as_rule() == Rule::enum_def {
                    enums.push(
                        parser
                            .parse_enum_def(child, metadata.clone(), is_public)
                            .unwrap(),
                    );
                }
            }
        }
    }
    assert_eq!(enums.len(), 1);
    let e = &enums[0];
    assert_eq!(e.name, "Foo");
    assert_eq!(e.variants.len(), 1);
    let v = &e.variants[0];
    assert_eq!(v.name, "Bar");
    assert!(
        v.has_no_mangle(),
        "variant Bar should have no_mangle from #[extern]"
    );
    assert!(v.is_extern(), "variant Bar should be extern");
}

#[test]
fn test_cross_module_extern_struct_collision() {
    use std::path::PathBuf;

    let mut registry = ModuleRegistry::new();
    let mod_a = ModulePath::from_parts(&["mod_a"]);
    let mod_b = ModulePath::from_parts(&["mod_b"]);

    let extern_meta = vec![Metadata {
        name: "extern".to_string(),
        args: vec![],
    }];

    fn make_mod(path: ModulePath, meta: Vec<Metadata>) -> Module {
        let mut program = crate::codegen::ast::Program::empty();
        program.module_path = Some(path.clone());
        program.structs = vec![StructDefinition {
            name: "Foo".to_string(),
            fields: vec![],
            is_public: true,
            metadata: meta,
        }];
        Module::new(path, PathBuf::from("test.kit"), vec![], vec![], program)
    }

    let first = make_mod(mod_a, extern_meta.clone());
    assert!(registry.register(first).is_ok());

    let second = make_mod(mod_b, extern_meta);
    let err = registry.register(second).unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("Foo") || err_str.contains("duplicate") || err_str.contains("Duplicate"),
        "expected DuplicateSymbol error, got: {err_str}"
    );
}

#[test]
fn test_cross_module_extern_enum_collision() {
    use std::path::PathBuf;

    let mut registry = ModuleRegistry::new();
    let mod_a = ModulePath::from_parts(&["mod_a"]);
    let mod_b = ModulePath::from_parts(&["mod_b"]);

    let extern_meta = vec![Metadata {
        name: "extern".to_string(),
        args: vec![],
    }];

    fn make_mod(path: ModulePath, meta: Vec<Metadata>) -> Module {
        let mut program = crate::codegen::ast::Program::empty();
        program.module_path = Some(path.clone());
        program.enums = vec![EnumDefinition {
            name: "MyEnum".to_string(),
            variants: vec![],
            is_public: true,
            metadata: meta,
        }];
        Module::new(path, PathBuf::from("test.kit"), vec![], vec![], program)
    }

    let first = make_mod(mod_a, extern_meta.clone());
    assert!(registry.register(first).is_ok());

    let second = make_mod(mod_b, extern_meta);
    let err = registry.register(second).unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("MyEnum")
            || err_str.contains("duplicate")
            || err_str.contains("Duplicate"),
        "expected DuplicateSymbol error, got: {err_str}"
    );
}
