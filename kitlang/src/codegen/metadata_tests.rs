use super::ast::{Function, GlobalDecl, MetaArg, Metadata, has_meta, has_no_mangle};
use super::frontend::Compiler;
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
        args: vec![MetaArg::Literal(super::ast::Literal::Int(42))],
    };
    assert_eq!(m.args.len(), 1);
    match &m.args[0] {
        MetaArg::Literal(lit) => assert_eq!(*lit, super::ast::Literal::Int(42)),
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
        body: super::ast::Block { stmts: vec![] },
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
        body: super::ast::Block { stmts: vec![] },
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
        body: super::ast::Block { stmts: vec![] },
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
    let mut registry = super::module::ModuleRegistry::new();
    assert!(registry.register_extern_name("myFunc").is_ok());
    assert!(registry.register_extern_name("myFunc").is_err());
}

#[test]
fn test_check_extern_name_distinct() {
    let mut registry = super::module::ModuleRegistry::new();
    assert!(registry.register_extern_name("funcA").is_ok());
    assert!(registry.register_extern_name("funcB").is_ok());
}

#[test]
fn test_extern_name_before_registration() {
    let registry = super::module::ModuleRegistry::new();
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

    let _ = std::fs::remove_dir_all(&dir);
}
