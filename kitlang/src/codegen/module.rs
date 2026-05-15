use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::PathBuf;

use crate::codegen::ast::{Include, Program};
use crate::codegen::type_ast::UsingClause;
use crate::error::{CompilationError, CompileResult};

/// A module path (e.g., `["pkg", "utils"]` -> `"pkg.utils"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModulePath(pub Vec<String>);

impl ModulePath {
    /// Create a new empty module path.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Create a module path from a slice of string parts.
    pub fn from_parts(parts: &[&str]) -> Self {
        Self(parts.iter().map(|s| s.to_string()).collect())
    }

    /// Returns `true` if this path has no components.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Join the path components with the given separator.
    pub fn join(&self, sep: &str) -> String {
        self.0.join(sep)
    }

    /// Append a single component to the end of this path.
    pub fn push(&mut self, part: String) {
        self.0.push(part);
    }

    /// Returns a slice over the underlying components.
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// Return the parent path (all components except the last).
    /// Returns an empty path if there is only one or zero components.
    pub fn parent(&self) -> Self {
        if self.0.len() <= 1 {
            Self::new()
        } else {
            Self(self.0[..self.0.len() - 1].to_vec())
        }
    }

    /// Returns `true` if this path starts with the given prefix.
    /// An empty prefix matches everything.
    pub fn starts_with(&self, other: &Self) -> bool {
        if other.is_empty() {
            return true;
        }
        self.0.len() >= other.0.len() && self.0[..other.0.len()] == other.0
    }
}

impl Default for ModulePath {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ModulePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.join("."))
    }
}

/// The type of import statement, matching the Haskell compiler's ImportType.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImportType {
    Single,
    Wildcard,
    DoubleWildcard,
}

/// Represents an import statement with source location tracking.
///
/// Analogous to the Haskell compiler's `(ModulePath, Span)` import representation.
/// The `span` field tracks the byte range of the import in the source file,
/// enabling precise error messages pointing to the failing import statement.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModuleImport {
    /// The module path being imported (e.g., `["pkg", "utils"]`).
    pub path: ModulePath,
    /// The type of import (single, wildcard, or double-wildcard).
    pub import_type: ImportType,
    /// Source location as byte offsets `(start, end)` within the source file, if known.
    pub span: Option<(usize, usize)>,
}

impl ModuleImport {
    pub fn new(path: ModulePath, import_type: ImportType) -> Self {
        Self {
            path,
            import_type,
            span: None,
        }
    }

    pub fn with_span(path: ModulePath, import_type: ImportType, span: (usize, usize)) -> Self {
        Self {
            path,
            import_type,
            span: Some(span),
        }
    }
}

/// A node in the module dependency graph.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModuleNode {
    pub path: ModulePath,
    pub source_path: PathBuf,
}

impl ModuleNode {
    pub fn new(path: ModulePath, source_path: PathBuf) -> Self {
        Self { path, source_path }
    }
}

/// A directed edge representing that module `from` depends on module `to`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DependencyEdge {
    /// Module making the dependency.
    pub from: ModulePath,
    /// Module being depended upon.
    pub to: ModulePath,
    /// Kind of import (single, wildcard, double-wildcard).
    pub import_type: ImportType,
}

/// The module dependency graph, used for topological sort and cycle detection.
#[derive(Clone, Debug, Default)]
pub struct DependencyGraph {
    nodes: HashMap<ModulePath, ModuleNode>,
    edges: Vec<DependencyEdge>,
    adjacency: HashMap<ModulePath, Vec<ModulePath>>,
    reverse_adjacency: HashMap<ModulePath, Vec<ModulePath>>,
}

impl DependencyGraph {
    /// Create an empty dependency graph.
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: Vec::new(),
            adjacency: HashMap::new(),
            reverse_adjacency: HashMap::new(),
        }
    }

    /// Insert a module node into the graph.
    pub fn add_node(&mut self, node: ModuleNode) {
        let path = node.path.clone();
        debug_assert!(!self.nodes.contains_key(&path), "duplicate node: {path}");
        self.nodes.entry(path.clone()).or_insert(node);
        self.adjacency.entry(path.clone()).or_default();
        self.reverse_adjacency.entry(path).or_default();
    }

    pub fn add_edge(&mut self, from: ModulePath, to: ModulePath, import_type: ImportType) {
        if from == to {
            return;
        }
        debug_assert!(
            self.nodes.contains_key(&from),
            "edge from unknown node: {}",
            from,
        );
        debug_assert!(self.nodes.contains_key(&to), "edge to unknown node: {}", to,);
        self.edges.push(DependencyEdge {
            from: from.clone(),
            to: to.clone(),
            import_type,
        });
        let to_key = to.clone();
        self.adjacency.entry(from.clone()).or_default().push(to_key);
        self.reverse_adjacency.entry(to).or_default().push(from);
    }

    pub fn contains_node(&self, path: &ModulePath) -> bool {
        self.nodes.contains_key(path)
    }

    pub fn get_node(&self, path: &ModulePath) -> Option<&ModuleNode> {
        self.nodes.get(path)
    }

    pub fn get_source_path(&self, path: &ModulePath) -> Option<&PathBuf> {
        self.nodes.get(path).map(|n| &n.source_path)
    }

    /// All modules that `path` directly depends on.
    pub fn dependencies_of(&self, path: &ModulePath) -> Vec<&ModulePath> {
        self.adjacency
            .get(path)
            .map(|deps| deps.iter().collect())
            .unwrap_or_default()
    }

    /// All modules that directly depend on `path`.
    pub fn dependents_of(&self, path: &ModulePath) -> Vec<&ModulePath> {
        self.reverse_adjacency
            .get(path)
            .map(|deps| deps.iter().collect())
            .unwrap_or_default()
    }

    /// Returns `true` if `path` has no outgoing edges (no dependencies).
    pub fn is_leaf_module(&self, path: &ModulePath) -> bool {
        self.adjacency
            .get(path)
            .map(|deps| deps.is_empty())
            .unwrap_or(true)
    }

    pub fn module_count(&self) -> usize {
        self.nodes.len()
    }

    /// All module paths stored in this graph.
    pub fn all_paths(&self) -> Vec<ModulePath> {
        self.nodes.keys().cloned().collect()
    }

    /// Sort modules so dependencies come before dependents.
    /// Returns `Err(CircularImport)` if the graph contains a cycle.
    pub fn topological_sort(&self) -> CompileResult<Vec<ModulePath>> {
        let mut remaining_deps: HashMap<&ModulePath, usize> = HashMap::new();
        for path in self.nodes.keys() {
            remaining_deps.entry(path).or_insert(0);
        }
        for (from, deps) in &self.adjacency {
            if !deps.is_empty() {
                *remaining_deps.entry(from).or_insert(0) += deps.len();
            }
        }

        let mut queue: VecDeque<&ModulePath> = VecDeque::new();
        for (path, degree) in &remaining_deps {
            if *degree == 0 {
                queue.push_back(path);
            }
        }

        let mut sorted: Vec<ModulePath> = Vec::new();
        while let Some(path) = queue.pop_front() {
            sorted.push(path.clone());
            self.dequeue_dependents(path, &mut remaining_deps, &mut queue);
        }

        if sorted.len() != self.nodes.len() {
            let missing: Vec<String> = self
                .nodes
                .keys()
                .filter(|p| !sorted.contains(p))
                .map(|p| p.to_string())
                .collect();
            return Err(CompilationError::CircularImport {
                cycle: missing.join(", "),
            });
        }

        // Verify topological ordering: each imported module must appear before the importer.
        debug_assert!(
            {
                let pos: HashMap<_, _> = sorted.iter().enumerate().map(|(i, p)| (p, i)).collect();
                self.edges.iter().all(|e| {
                    pos.get(&e.from)
                        .zip(pos.get(&e.to))
                        .map(|(from_pos, to_pos)| from_pos > to_pos)
                        .unwrap_or(true)
                })
            },
            "topological_sort produced invalid order",
        );

        Ok(sorted)
    }

    fn dequeue_dependents<'a>(
        &'a self,
        path: &ModulePath,
        remaining_deps: &mut HashMap<&'a ModulePath, usize>,
        queue: &mut VecDeque<&'a ModulePath>,
    ) {
        let Some(dependents) = self.reverse_adjacency.get(path) else {
            return;
        };
        for depender in dependents {
            let Some(degree) = remaining_deps.get_mut(depender) else {
                continue;
            };
            *degree = degree.saturating_sub(1);
            if *degree == 0 {
                queue.push_back(depender);
            }
        }
    }

    /// Returns all directed cycles in the dependency graph.
    /// Each cycle is represented as a list of module paths forming a closed loop.
    pub fn detect_cycles(&self) -> Vec<Vec<ModulePath>> {
        let mut cycles = Vec::new();
        let mut visited = HashSet::new();
        let mut path_stack = Vec::new();

        for path in self.nodes.keys() {
            if !visited.contains(path) {
                let mut path_set = HashSet::new();
                self.detect_cycles_dfs(
                    path,
                    &mut visited,
                    &mut path_stack,
                    &mut path_set,
                    &mut cycles,
                );
            }
        }
        cycles
    }

    fn detect_cycles_dfs(
        &self,
        path: &ModulePath,
        global_visited: &mut HashSet<ModulePath>,
        path_stack: &mut Vec<ModulePath>,
        path_set: &mut HashSet<ModulePath>,
        cycles: &mut Vec<Vec<ModulePath>>,
    ) {
        if path_set.contains(path) {
            let start = path_stack.iter().position(|p| p == path).unwrap();
            cycles.push(path_stack[start..].to_vec());
            return;
        }
        if global_visited.contains(path) {
            return;
        }
        global_visited.insert(path.clone());
        path_stack.push(path.clone());
        path_set.insert(path.clone());

        if let Some(deps) = self.adjacency.get(path) {
            for dep in deps {
                self.detect_cycles_dfs(dep, global_visited, path_stack, path_set, cycles);
            }
        }

        path_stack.pop();
        path_set.remove(path);
    }
}

/// The kind of binding a name refers to in module-level name resolution.
///
/// Analogous to the Haskell compiler's `Binding` type with `ModuleBinding` variant.
#[derive(Clone, Debug, PartialEq)]
pub enum NameBinding {
    Module(ModulePath),
    Extern,
}

/// The kind of declaration a name refers to in module-level name resolution.
///
/// Analogous to the Haskell compiler's `SyntacticBinding` variants:
/// `TypeBinding`, `FunctionBinding`, `VarBinding`, etc.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DeclKind {
    Function,
    Global,
    Struct,
    Enum,
    Trait,
    RuleSet,
}

/// A declaration binding: a name declared in a specific module with a specific kind.
///
/// Used for cross-module name resolution to determine which module defines
/// a name and what kind of declaration it is (function, struct, etc.).
#[derive(Clone, Debug, PartialEq)]
pub struct DeclBinding {
    /// The module that declares this name.
    pub module: ModulePath,
    /// What kind of declaration it is.
    pub kind: DeclKind,
}

/// Stores name bindings for a single module's scope.
#[derive(Clone, Debug, Default)]
pub struct BindingTable {
    bindings: HashMap<String, NameBinding>,
}

impl BindingTable {
    pub fn new() -> Self {
        Self {
            bindings: HashMap::new(),
        }
    }

    /// Insert a name binding; returns an error if the name already exists with a different binding.
    pub fn insert(&mut self, name: String, binding: NameBinding) -> Result<(), CompilationError> {
        if let Some(existing) = self.bindings.get(&name)
            && *existing != binding
        {
            let module_str = match existing {
                NameBinding::Module(p) => p.to_string(),
                NameBinding::Extern => "extern".to_string(),
            };
            return Err(CompilationError::DuplicateSymbol {
                name,
                module: module_str,
            });
        }
        self.bindings.insert(name, binding);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&NameBinding> {
        self.bindings.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.bindings.contains_key(name)
    }
}

/// Represents a loaded module, analogous to the Haskell compiler's `Module` type.
#[derive(Clone, Debug)]
pub struct Module {
    pub path: ModulePath,
    pub source_path: PathBuf,
    pub imports: Vec<ModuleImport>,
    pub includes: Vec<Include>,
    pub program: Program,
    pub is_c_module: bool,
    /// `using` statements for implicit conversions and rule set imports.
    pub mod_using: Vec<UsingClause>,
}

impl Module {
    pub fn new(
        path: ModulePath,
        source_path: PathBuf,
        imports: Vec<ModuleImport>,
        includes: Vec<Include>,
        program: Program,
    ) -> Self {
        Self {
            path,
            source_path,
            imports,
            includes,
            program,
            is_c_module: false,
            mod_using: Vec::new(),
        }
    }
}

/// Central registry for all loaded modules.
/// Provides lookup, dependency graph construction, and topological sorting.
#[derive(Clone, Debug, Default)]
pub struct ModuleRegistry {
    modules: HashMap<ModulePath, Module>,
    graph: DependencyGraph,
    bindings: BindingTable,
    /// Maps declaration names to their bindings (module + kind) for name resolution.
    declarations: HashMap<String, Vec<DeclBinding>>,
    /// Modules that failed to load. Used to avoid cascading "dependency not found" errors.
    pub(crate) failed: HashSet<ModulePath>,
}

impl ModuleRegistry {
    /// Create an empty registry with no modules.
    pub fn new() -> Self {
        Self {
            modules: HashMap::new(),
            graph: DependencyGraph::new(),
            bindings: BindingTable::new(),
            declarations: HashMap::new(),
            failed: HashSet::new(),
        }
    }

    /// Register a module and its declarations into the registry.
    /// Modules are looked up by path; duplicate paths are overwritten.
    /// Register a module and its declarations into the registry.
    /// Panics in debug builds if a module with the same path is already registered.
    pub fn register(&mut self, module: Module) {
        let path = module.path.clone();
        let source_path = module.source_path.clone();
        debug_assert!(
            !self.modules.contains_key(&path),
            "duplicate module: {}",
            path,
        );
        self.graph
            .add_node(ModuleNode::new(path.clone(), source_path));
        self.register_module_declarations(&module);
        let _ = self.register_module_binding(&module);
        self.modules.insert(path, module);
    }

    /// Register a single declaration name in the global declaration table.
    fn register_decl(&mut self, name: String, kind: DeclKind, mod_path: &ModulePath) {
        debug_assert!(!name.is_empty(), "empty declaration name");
        self.declarations
            .entry(name)
            .or_default()
            .push(DeclBinding {
                module: mod_path.clone(),
                kind,
            });
    }

    fn register_module_declarations(&mut self, module: &Module) {
        let p = &module.path;
        for f in &module.program.functions {
            self.register_decl(f.name.clone(), DeclKind::Function, p);
        }
        for g in &module.program.globals {
            self.register_decl(g.name.clone(), DeclKind::Global, p);
        }
        for s in &module.program.structs {
            self.register_decl(s.name.clone(), DeclKind::Struct, p);
        }
        for e in &module.program.enums {
            self.register_decl(e.name.clone(), DeclKind::Enum, p);
        }
        for t in &module.program.traits {
            self.register_decl(t.name.clone(), DeclKind::Trait, p);
        }
        for r in &module.program.rulesets {
            self.register_decl(r.name.clone(), DeclKind::RuleSet, p);
        }
    }

    fn register_module_binding(&mut self, module: &Module) -> CompileResult<()> {
        let mut accumulated = ModulePath::new();
        for component in module.path.as_slice() {
            accumulated.push(component.clone());
            self.bindings.insert(
                accumulated.join("."),
                NameBinding::Module(accumulated.clone()),
            )?
        }
        Ok(())
    }

    /// Look up a name binding registered with `register_module_binding`.
    pub fn lookup_binding(&self, name: &str) -> Option<&NameBinding> {
        self.bindings.get(name)
    }

    /// Find which module defines a given declaration name.
    /// Prefers the current module if it also defines the name.
    /// Find the module that declares a given name, preferring the current module.
    ///
    /// When multiple modules declare the same name, the current module's
    /// declaration takes priority. Falls back to the first registered module.
    pub fn find_module_for_declaration(
        &self,
        name: &str,
        current_module: &ModulePath,
    ) -> Option<ModulePath> {
        let candidates = self.declarations.get(name)?;
        for decl in candidates {
            if decl.module == *current_module {
                return Some(current_module.clone());
            }
        }
        candidates.first().map(|d| d.module.clone())
    }

    /// Check whether an extern-visible name has already been registered by another module.
    /// Returns `DuplicateSymbol` if the name already exists in the binding table.
    pub fn check_extern_name(&self, name: &str) -> Result<(), CompilationError> {
        let extern_key = format!("extern.{}", name);
        if self.bindings.contains(&extern_key) {
            return Err(CompilationError::DuplicateSymbol {
                name: name.to_string(),
                module: "extern (global namespace)".to_string(),
            });
        }
        Ok(())
    }

    /// Register an extern-visible name in the binding table to prevent duplicates.
    pub fn register_extern_name(&mut self, name: &str) -> Result<(), CompilationError> {
        let extern_key = format!("extern.{}", name);
        self.bindings.insert(extern_key, NameBinding::Extern)
    }

    /// Find the declaration kind for a name in a specific module, if known.
    pub fn find_decl_kind(&self, name: &str, module: &ModulePath) -> Option<DeclKind> {
        let candidates = self.declarations.get(name)?;
        candidates
            .iter()
            .find(|d| d.module == *module)
            .map(|d| d.kind)
    }

    /// Resolve a potentially qualified name into (module_path, base_name).
    /// `foo.bar.baz` -> module `["foo", "bar"]`, name `"baz"`
    /// `baz` -> uses `find_module_for_declaration` with the current module hint.
    pub fn resolve_qualified_name(
        &self,
        name: &str,
        current_module: &ModulePath,
    ) -> Option<(ModulePath, String)> {
        let parts: Vec<&str> = name.split('.').collect();
        if parts.len() == 1 {
            let mod_path = self.find_module_for_declaration(name, current_module)?;
            Some((mod_path, name.to_string()))
        } else {
            let base_name = parts.last()?.to_string();
            let mod_segments: Vec<String> = parts[..parts.len() - 1]
                .iter()
                .map(|s| s.to_string())
                .collect();
            let mod_path = ModulePath(mod_segments);
            if self.modules.contains_key(&mod_path) {
                debug_assert!(!base_name.is_empty(), "empty base name in qualified name");
                Some((mod_path, base_name))
            } else {
                self.find_module_for_declaration(name, current_module)
                    .map(|m| (m, name.to_string()))
            }
        }
    }

    /// Finalize the dependency graph by adding edges from registered import statements.
    /// Returns `Err(CircularImport)` if a cycle is detected.
    pub fn finalize_graph(&mut self) -> CompileResult<()> {
        let paths: Vec<ModulePath> = self.modules.keys().cloned().collect();
        for path in &paths {
            if let Some(module) = self.modules.get(path) {
                for import in &module.imports {
                    if self.modules.contains_key(&import.path) {
                        self.graph
                            .add_edge(path.clone(), import.path.clone(), import.import_type);
                    }
                }
            }
        }

        let cycles = self.graph.detect_cycles();
        if !cycles.is_empty() {
            let desc: Vec<String> = cycles
                .iter()
                .map(|c| {
                    c.iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(" -> ")
                })
                .collect();
            return Err(CompilationError::CircularImport {
                cycle: desc.join("; "),
            });
        }
        Ok(())
    }

    pub fn get(&self, path: &ModulePath) -> Option<&Module> {
        self.modules.get(path)
    }

    pub fn contains(&self, path: &ModulePath) -> bool {
        self.modules.contains_key(path)
    }

    pub fn graph(&self) -> &DependencyGraph {
        &self.graph
    }

    pub fn topological_sort(&self) -> CompileResult<Vec<ModulePath>> {
        self.graph.topological_sort()
    }

    /// All registered modules (borrowed references).
    pub fn all_modules(&self) -> Vec<&Module> {
        self.modules.values().collect()
    }

    /// All registered modules (owned values).
    pub fn into_modules(self) -> Vec<Module> {
        self.modules.into_values().collect()
    }

    pub fn module_count(&self) -> usize {
        self.modules.len()
    }

    /// Collect all unique include directives across all modules.
    pub fn collect_all_includes(&self) -> Vec<Include> {
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        for module in self.modules.values() {
            for inc in &module.includes {
                if seen.insert(inc.path.clone()) {
                    result.push(inc.clone());
                }
            }
        }
        result
    }
}

#[cfg(test)]
#[path = "module_tests.rs"]
mod tests;
