use super::ast::{
    Attributed, Function, GlobalDecl, Literal, MetaArg, Metadata, has_meta, has_no_mangle,
};
use super::ast::{Block, Program};
use super::frontend::Compiler;
use super::module::{Module, ModulePath, ModuleRegistry};
use super::parser::Parser;
use super::type_ast::{EnumDefinition, EnumVariant, StructDefinition};
use super::types::TypeId;
use crate::KitParser;
use crate::Rule;
use crate::error::CompilationError;
use pest::Parser as PestParser;

use std::{
    env,
    fs::{self, File},
    path::PathBuf,
};

fn meta(name: &str) -> Vec<Metadata> {
    vec![Metadata {
        name: name.to_string(),
        args: vec![],
    }]
}

fn assert_no_mangle<T: Attributed>(item: &T, expect_no_mangle: bool, expect_extern: bool) {
    assert_eq!(item.has_no_mangle(), expect_no_mangle);
    assert_eq!(item.is_extern(), expect_extern);
}

fn parse_one_function(source: &str) -> Function {
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    pairs
        .filter(|p| p.as_rule() == Rule::function_decl)
        .map(|p| parser.parse_function(p).unwrap())
        .next()
        .expect("expected one function_decl")
}

fn parse_one_global(source: &str) -> GlobalDecl {
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    pairs
        .filter(|p| p.as_rule() == Rule::var_decl)
        .map(|p| parser.parse_global_var_decl(&p).unwrap())
        .next()
        .expect("expected one var_decl")
}

fn parse_one_struct(source: &str) -> StructDefinition {
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    for pair in pairs {
        if pair.as_rule() == Rule::type_def {
            let mut inner = pair.into_inner();
            let (metadata, is_public) = Parser::parse_metadata_and_modifiers(inner.next());
            for child in inner {
                if child.as_rule() == Rule::struct_def {
                    return parser.parse_struct_def(child, metadata, is_public).unwrap();
                }
            }
        }
    }
    panic!("expected one struct_def")
}

fn parse_one_enum(source: &str) -> EnumDefinition {
    let pairs = KitParser::parse(Rule::program, source).unwrap();
    let parser = Parser::new();
    for pair in pairs {
        if pair.as_rule() == Rule::type_def {
            let mut inner = pair.into_inner();
            let (metadata, is_public) = Parser::parse_metadata_and_modifiers(inner.next());
            for child in inner {
                if child.as_rule() == Rule::enum_def {
                    return parser.parse_enum_def(child, metadata, is_public).unwrap();
                }
            }
        }
    }
    panic!("expected one enum_def")
}

fn make_extern_mod(path: ModulePath, name: &str) -> Module {
    let mut program = Program::empty();
    program.module_path = Some(path.clone());
    program.functions = vec![Function {
        name: name.to_string(),
        params: vec![],
        return_type: None,
        inferred_return: None,
        body: Block { stmts: vec![] },
        is_public: true,
        metadata: meta("extern"),
    }];
    Module::new(
        path,
        PathBuf::from(format!("{name}.kit")),
        vec![],
        vec![],
        program,
    )
}

fn make_extern_struct_mod(path: ModulePath) -> Module {
    let mut program = Program::empty();
    program.module_path = Some(path.clone());
    program.structs = vec![StructDefinition {
        name: "Foo".to_string(),
        fields: vec![],
        is_public: true,
        metadata: meta("extern"),
    }];
    Module::new(path, PathBuf::from("test.kit"), vec![], vec![], program)
}

fn make_extern_enum_mod(path: ModulePath) -> Module {
    let mut program = Program::empty();
    program.module_path = Some(path.clone());
    program.enums = vec![EnumDefinition {
        name: "MyEnum".to_string(),
        variants: vec![],
        is_public: true,
        metadata: meta("extern"),
    }];
    Module::new(path, PathBuf::from("test.kit"), vec![], vec![], program)
}

fn assert_duplicate_error(result: Result<(), CompilationError>, name: &str) {
    let err = result.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains(name) || err_str.contains("duplicate") || err_str.contains("Duplicate"),
        "expected error mentioning '{name}' or 'duplicate', got: {err_str}"
    );
}

// --- Metadata primitive tests ---

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
    assert!(has_no_mangle(&meta("extern")));
    assert!(has_no_mangle(&meta("expose")));
    assert!(!has_no_mangle(&meta("inline")));
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
        MetaArg::Literal(_) => panic!("expected identifier"),
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
        MetaArg::Identifier(_) => panic!("expected literal"),
    }
}

// --- Attributed property tests ---

#[test]
fn test_function_has_no_mangle_extern() {
    let f = Function {
        name: "foo".to_string(),
        params: vec![],
        return_type: None,
        inferred_return: None,
        body: Block { stmts: vec![] },
        is_public: true,
        metadata: meta("extern"),
    };
    assert_no_mangle(&f, true, true);
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
        metadata: meta("expose"),
    };
    assert_no_mangle(&f, true, false);
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
    assert_no_mangle(&f, false, false);
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
        metadata: meta("extern"),
    };
    assert_no_mangle(&g, true, true);
}

#[test]
fn test_struct_has_no_mangle_extern() {
    let s = StructDefinition {
        name: "Foo".to_string(),
        fields: vec![],
        is_public: true,
        metadata: meta("extern"),
    };
    assert_no_mangle(&s, true, true);
}

#[test]
fn test_struct_has_no_mangle_expose() {
    let s = StructDefinition {
        name: "Foo".to_string(),
        fields: vec![],
        is_public: true,
        metadata: meta("expose"),
    };
    assert_no_mangle(&s, true, false);
}

#[test]
fn test_struct_no_metadata() {
    let s = StructDefinition {
        name: "Foo".to_string(),
        fields: vec![],
        is_public: true,
        metadata: vec![],
    };
    assert_no_mangle(&s, false, false);
}

#[test]
fn test_enum_has_no_mangle_extern() {
    let e = EnumDefinition {
        name: "Foo".to_string(),
        variants: vec![],
        is_public: true,
        metadata: meta("extern"),
    };
    assert_no_mangle(&e, true, true);
}

#[test]
fn test_enum_variant_has_no_mangle_extern() {
    let v = EnumVariant {
        name: "Bar".to_string(),
        parent: "Foo".to_string(),
        args: vec![],
        default: None,
        metadata: meta("extern"),
    };
    assert_no_mangle(&v, true, true);
}

#[test]
fn test_enum_variant_has_no_mangle_expose() {
    let v = EnumVariant {
        name: "Bar".to_string(),
        parent: "Foo".to_string(),
        args: vec![],
        default: None,
        metadata: meta("expose"),
    };
    assert_no_mangle(&v, true, false);
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
    assert_no_mangle(&v, false, false);
}

// --- Registration tests ---

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
    assert!(registry.check_extern_name("newFunc").is_ok());
}

// --- Parse tests: function/global ---

#[test]
fn test_parse_extern_function() {
    let f = parse_one_function("#[extern] function externFunc() { printf(\"hello\"); }\n");
    assert_eq!(f.name, "externFunc");
    assert_no_mangle(&f, true, true);
}

#[test]
fn test_parse_expose_function() {
    let f = parse_one_function("#[expose] function exposedFunc() { }\n");
    assert_eq!(f.name, "exposedFunc");
    assert_no_mangle(&f, true, false);
}

#[test]
fn test_parse_private_function() {
    let f = parse_one_function("private function helper() { }\n");
    assert_eq!(f.name, "helper");
    assert!(!f.is_public);
}

#[test]
fn test_parse_public_function() {
    let f = parse_one_function("public function pubFunc() { }\n");
    assert_eq!(f.name, "pubFunc");
    assert!(f.is_public);
}

#[test]
fn test_parse_function_default_public() {
    let f = parse_one_function("function defaultPublic() { }\n");
    assert!(f.is_public);
    assert_no_mangle(&f, false, false);
}

#[test]
fn test_parse_extern_global() {
    let g = parse_one_global("#[extern] var globalExtern: int = 42;\n");
    assert_eq!(g.name, "globalExtern");
    assert_no_mangle(&g, true, true);
}

#[test]
fn test_parse_expose_global() {
    let g = parse_one_global("#[expose] var globalExpose: int = 7;\n");
    assert_eq!(g.name, "globalExpose");
    assert_no_mangle(&g, true, false);
}

#[test]
fn test_parse_private_global() {
    let g = parse_one_global("private var privGlobal: int = 0;\n");
    assert_eq!(g.name, "privGlobal");
    assert!(!g.is_public);
}

#[test]
fn test_parse_extern_with_args() {
    let f = parse_one_function("#[extern(\"C\")] function externWithArg() { }\n");
    assert_eq!(f.name, "externWithArg");
    assert_no_mangle(&f, true, true);
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

// --- Parse tests: type_def (struct/enum/enum variant) ---

#[test]
fn test_parse_extern_struct() {
    let s = parse_one_struct("#[extern] struct ExternStruct { var x: int; }\n");
    assert_eq!(s.name, "ExternStruct");
    assert_no_mangle(&s, true, true);
}

#[test]
fn test_parse_expose_struct() {
    let s = parse_one_struct("#[expose] struct ExposeStruct { var x: int; }\n");
    assert_eq!(s.name, "ExposeStruct");
    assert_no_mangle(&s, true, false);
}

#[test]
fn test_parse_extern_enum() {
    let e = parse_one_enum("#[extern] enum ExternEnum { A; B; }\n");
    assert_eq!(e.name, "ExternEnum");
    assert_no_mangle(&e, true, true);
}

#[test]
fn test_parse_expose_enum() {
    let e = parse_one_enum("#[expose] enum ExposeEnum { A; B; }\n");
    assert_eq!(e.name, "ExposeEnum");
    assert_no_mangle(&e, true, false);
}

#[test]
fn test_parse_extern_enum_variant() {
    let e = parse_one_enum("enum Foo { #[extern] Bar(x: int); }\n");
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

// --- Cross-module collision tests ---

#[test]
fn test_cross_module_extern_collision() {
    let mut registry = ModuleRegistry::new();
    let mod_a = ModulePath::from_parts(&["mod_a"]);
    let mod_b = ModulePath::from_parts(&["mod_b"]);

    let first = make_extern_mod(mod_a, "foo");
    assert!(registry.register(first).is_ok());

    let second = make_extern_mod(mod_b, "foo");
    assert_duplicate_error(registry.register(second), "foo");
}

#[test]
fn test_cross_module_extern_struct_collision() {
    let mut registry = ModuleRegistry::new();
    let mod_a = ModulePath::from_parts(&["mod_a"]);
    let mod_b = ModulePath::from_parts(&["mod_b"]);

    let first = make_extern_struct_mod(mod_a);
    assert!(registry.register(first).is_ok());

    let second = make_extern_struct_mod(mod_b);
    assert_duplicate_error(registry.register(second), "Foo");
}

#[test]
fn test_cross_module_extern_enum_collision() {
    let mut registry = ModuleRegistry::new();
    let mod_a = ModulePath::from_parts(&["mod_a"]);
    let mod_b = ModulePath::from_parts(&["mod_b"]);

    let first = make_extern_enum_mod(mod_a);
    assert!(registry.register(first).is_ok());

    let second = make_extern_enum_mod(mod_b);
    assert_duplicate_error(registry.register(second), "MyEnum");
}

// --- Integration: extern C output ---

#[test]
fn test_extern_c_output_contains_extern_prefix() {
    use std::io::Write;

    let dir = env::temp_dir().join("kit_test_extern_output");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let kit_path = dir.join("test_extern.kit");
    let mut file = File::create(&kit_path).unwrap();
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

    let mut compiler = Compiler::new(
        vec![kit_path],
        dir.join("test_extern"),
        vec![],
        &[] as &[String],
    );

    unsafe { env::set_var("KEEP_C", "1") };
    compiler.compile().unwrap();
    unsafe { env::remove_var("KEEP_C") };

    let c_path = dir.join("test_extern_modules").join("test_extern.c");
    let c_code = fs::read_to_string(&c_path).unwrap_or_else(|e| {
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

    let _ = fs::remove_dir_all(&dir);
}
