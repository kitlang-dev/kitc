use super::*;
use crate::codegen::{
    ast::{Block, Function, GlobalDecl, Program},
    types::TypeId,
};

#[test]
fn test_module_path_basics() {
    let empty = ModulePath::new();
    assert!(empty.is_empty());
    assert_eq!(empty.join("."), "");

    let simple = ModulePath::from_parts(&["foo", "bar"]);
    assert!(!simple.is_empty());
    assert_eq!(simple.join("."), "foo.bar");
    assert_eq!(simple.join("_"), "foo_bar");
}

#[test]
fn test_module_path_parent() {
    let path = ModulePath::from_parts(&["a", "b", "c"]);
    let parent = path.parent();
    assert_eq!(parent.join("."), "a.b");
    let grandparent = parent.parent();
    assert_eq!(grandparent.join("."), "a");
    let root = grandparent.parent();
    assert!(root.is_empty());
}

#[test]
fn test_module_path_starts_with() {
    let path = ModulePath::from_parts(&["std", "io", "file"]);
    assert!(path.starts_with(&ModulePath::from_parts(&["std"])));
    assert!(path.starts_with(&ModulePath::from_parts(&["std", "io"])));
    assert!(!path.starts_with(&ModulePath::from_parts(&["core"])));
}

fn build_linear_graph() -> DependencyGraph {
    let mut graph = DependencyGraph::new();
    graph.add_node(ModuleNode::new(mp("a"), PathBuf::from("a.kit")));
    graph.add_node(ModuleNode::new(mp("b"), PathBuf::from("b.kit")));
    graph.add_node(ModuleNode::new(mp("c"), PathBuf::from("c.kit")));
    graph.add_edge(mp("a"), mp("b"), ImportType::Single);
    graph.add_edge(mp("b"), mp("c"), ImportType::Single);
    graph
}

#[test]
fn test_dependency_graph_empty() {
    let graph = DependencyGraph::new();
    assert_eq!(graph.module_count(), 0);
    assert!(graph.topological_sort().unwrap().is_empty());
}

#[test]
fn test_dependency_graph_single_node() {
    let mut graph = DependencyGraph::new();
    graph.add_node(ModuleNode::new(mp("main"), PathBuf::from("main.kit")));
    assert_eq!(graph.module_count(), 1);
    let sorted = graph.topological_sort().unwrap();
    assert_eq!(sorted.len(), 1);
    assert_eq!(sorted[0], mp("main"));
}

#[test]
fn test_dependency_graph_linear() {
    let graph = build_linear_graph();
    let sorted = graph.topological_sort().unwrap();
    assert_eq!(sorted.len(), 3);
    assert!(sorted.iter().position(|p| *p == mp("c")) < sorted.iter().position(|p| *p == mp("b")));
    assert!(sorted.iter().position(|p| *p == mp("b")) < sorted.iter().position(|p| *p == mp("a")));
}

#[test]
fn test_dependency_graph_cycle_detection() {
    let mut graph = build_linear_graph();
    graph.add_edge(mp("c"), mp("a"), ImportType::Single);
    assert!(!graph.detect_cycles().is_empty());
    assert!(graph.topological_sort().is_err());
}

#[test]
fn test_dependency_graph_diamond() {
    let mut graph = DependencyGraph::new();
    for name in &["main", "lib1", "lib2", "base"] {
        graph.add_node(ModuleNode::new(
            mp(name),
            PathBuf::from(format!("{name}.kit")),
        ));
    }
    graph.add_edge(mp("main"), mp("lib1"), ImportType::Single);
    graph.add_edge(mp("main"), mp("lib2"), ImportType::Single);
    graph.add_edge(mp("lib1"), mp("base"), ImportType::Single);
    graph.add_edge(mp("lib2"), mp("base"), ImportType::Single);

    let sorted = graph.topological_sort().unwrap();
    assert_eq!(sorted.len(), 4);
    assert!(
        sorted.iter().position(|p| *p == mp("base")) < sorted.iter().position(|p| *p == mp("main"))
    );
}

#[test]
fn test_dependency_graph_self_edge() {
    let mut graph = DependencyGraph::new();
    graph.add_node(ModuleNode::new(mp("s"), PathBuf::from("s.kit")));
    graph.add_edge(mp("s"), mp("s"), ImportType::Single);
    assert_eq!(graph.topological_sort().unwrap().len(), 1);
}

#[test]
fn test_declaration_registration() {
    let mut registry = ModuleRegistry::new();
    let mod_path = ModulePath::from_parts(&["test_mod"]);

    let mut program = Program::empty();
    program.module_path = Some(mod_path.clone());
    program.globals = vec![GlobalDecl {
        name: "GLOBAL".to_string(),
        annotation: None,
        inferred: TypeId::default(),
        init: None,
        is_const: false,
        is_public: true,
        metadata: vec![],
    }];
    program.functions = vec![Function {
        name: "my_func".to_string(),
        params: vec![],
        return_type: None,
        inferred_return: None,
        body: Block { stmts: vec![] },
        is_public: true,
        metadata: vec![],
    }];

    let module = Module::new(
        mod_path.clone(),
        PathBuf::from("test_mod.kit"),
        vec![],
        vec![],
        program,
    );
    registry.register(module).unwrap();

    assert_eq!(
        registry.find_module_for_declaration("my_func", &ModulePath::new()),
        Some(mod_path.clone())
    );
    assert_eq!(
        registry.find_module_for_declaration("GLOBAL", &ModulePath::new()),
        Some(mod_path.clone())
    );
    assert_eq!(
        registry.find_module_for_declaration("nonexistent", &ModulePath::new()),
        None
    );
    assert_eq!(
        registry.find_decl_kind("my_func", &mod_path),
        Some(DeclKind::Function),
    );
    assert_eq!(
        registry.find_decl_kind("GLOBAL", &mod_path),
        Some(DeclKind::Global),
    );
}

#[test]
fn test_resolve_qualified_name_simple() {
    let mut registry = ModuleRegistry::new();
    let mod_path = ModulePath::from_parts(&["math"]);

    let mut program = Program::empty();
    program.module_path = Some(mod_path.clone());
    program.functions = vec![Function {
        name: "add".to_string(),
        params: vec![],
        return_type: None,
        inferred_return: None,
        body: Block { stmts: vec![] },
        is_public: true,
        metadata: vec![],
    }];
    registry
        .register(Module::new(
            mod_path.clone(),
            PathBuf::from("math.kit"),
            vec![],
            vec![],
            program,
        ))
        .unwrap();

    let (found_mod, found_name) = registry
        .resolve_qualified_name("add", &ModulePath::new())
        .unwrap();
    assert_eq!(found_mod, mod_path);
    assert_eq!(found_name, "add");
}

#[test]
fn test_resolve_qualified_name_dotted() {
    let mut registry = ModuleRegistry::new();
    let mod_path = ModulePath::from_parts(&["pkg", "math"]);

    let mut program = Program::empty();
    program.module_path = Some(mod_path.clone());
    program.functions = vec![Function {
        name: "add".to_string(),
        params: vec![],
        return_type: None,
        inferred_return: None,
        body: Block { stmts: vec![] },
        is_public: true,
        metadata: vec![],
    }];
    registry
        .register(Module::new(
            mod_path.clone(),
            PathBuf::from("pkg/math.kit"),
            vec![],
            vec![],
            program,
        ))
        .unwrap();

    let (found_mod, found_name) = registry
        .resolve_qualified_name("pkg.math.add", &ModulePath::new())
        .unwrap();
    assert_eq!(found_mod, mod_path);
    assert_eq!(found_name, "add");
}

/// Shorthand for ModulePath::from_parts(&[s])
fn mp(s: &str) -> ModulePath {
    ModulePath::from_parts(&[s])
}
