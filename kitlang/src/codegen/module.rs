use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::PathBuf;

use crate::codegen::ast::{Include, Program};
use crate::error::{CompilationError, CompileResult};

/// A module path (e.g., `["pkg", "utils"]` -> `"pkg.utils"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModulePath(pub Vec<String>);

impl ModulePath {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn from_parts(parts: &[&str]) -> Self {
        Self(parts.iter().map(|s| s.to_string()).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn join(&self, sep: &str) -> String {
        self.0.join(sep)
    }

    pub fn push(&mut self, part: String) {
        self.0.push(part);
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub fn parent(&self) -> Self {
        if self.0.len() <= 1 {
            Self::new()
        } else {
            Self(self.0[..self.0.len() - 1].to_vec())
        }
    }

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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ImportType {
    Single,
    Wildcard,
    DoubleWildcard,
}

/// Represents an import statement.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModuleImport {
    /// The module path being imported (e.g., `["pkg", "utils"]`).
    pub path: ModulePath,
    /// The type of import (single, wildcard, or double-wildcard).
    pub import_type: ImportType,
}

impl ModuleImport {
    pub fn new(path: ModulePath, import_type: ImportType) -> Self {
        Self { path, import_type }
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

/// Directed edge representing a dependency between modules.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DependencyEdge {
    pub from: ModulePath,
    pub to: ModulePath,
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
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: Vec::new(),
            adjacency: HashMap::new(),
            reverse_adjacency: HashMap::new(),
        }
    }

    pub fn add_node(&mut self, node: ModuleNode) {
        let path = node.path.clone();
        self.nodes.entry(path.clone()).or_insert(node);
        self.adjacency.entry(path.clone()).or_default();
        self.reverse_adjacency.entry(path).or_default();
    }

    pub fn add_edge(&mut self, from: ModulePath, to: ModulePath, import_type: ImportType) {
        if from == to {
            return;
        }
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

    pub fn dependencies_of(&self, path: &ModulePath) -> Vec<&ModulePath> {
        self.adjacency
            .get(path)
            .map(|deps| deps.iter().collect())
            .unwrap_or_default()
    }

    pub fn dependents_of(&self, path: &ModulePath) -> Vec<&ModulePath> {
        self.reverse_adjacency
            .get(path)
            .map(|deps| deps.iter().collect())
            .unwrap_or_default()
    }

    pub fn is_leaf_module(&self, path: &ModulePath) -> bool {
        self.adjacency
            .get(path)
            .map(|deps| deps.is_empty())
            .unwrap_or(true)
    }

    pub fn module_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn all_paths(&self) -> Vec<ModulePath> {
        self.nodes.keys().cloned().collect()
    }

    /// Topological sort using Kahn's algorithm with cycle detection.
    /// Returns modules in dependency-first order (dependencies before dependents).
    pub fn topological_sort(&self) -> CompileResult<Vec<ModulePath>> {
        let mut out_degree: HashMap<&ModulePath, usize> = HashMap::new();
        for path in self.nodes.keys() {
            out_degree.entry(path).or_insert(0);
        }
        for (from, deps) in &self.adjacency {
            if !deps.is_empty() {
                *out_degree.entry(from).or_insert(0) += deps.len();
            }
        }

        let mut queue: VecDeque<&ModulePath> = VecDeque::new();
        for (path, degree) in &out_degree {
            if *degree == 0 {
                queue.push_back(path);
            }
        }

        let mut sorted: Vec<ModulePath> = Vec::new();
        while let Some(path) = queue.pop_front() {
            sorted.push(path.clone());
            self.dequeue_dependents(path, &mut out_degree, &mut queue);
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

        Ok(sorted)
    }

    fn dequeue_dependents<'a>(
        &'a self,
        path: &ModulePath,
        out_degree: &mut HashMap<&'a ModulePath, usize>,
        queue: &mut VecDeque<&'a ModulePath>,
    ) {
        let Some(dependents) = self.reverse_adjacency.get(path) else {
            return;
        };
        for depender in dependents {
            let Some(degree) = out_degree.get_mut(depender) else {
                continue;
            };
            *degree = degree.saturating_sub(1);
            if *degree == 0 {
                queue.push_back(depender);
            }
        }
    }

    /// Detect and return any cycles in the dependency graph.
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

/// A map from simple names to their bindings within a module namespace.
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

    pub fn insert(&mut self, name: String, binding: NameBinding) -> Result<(), CompilationError> {
        if let Some(existing) = self.bindings.get(&name)
            && *existing != binding
        {
            return Err(CompilationError::DuplicateSymbol {
                name,
                module: format!("{:?}", existing),
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
    declarations: HashMap<String, HashSet<ModulePath>>,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self {
            modules: HashMap::new(),
            graph: DependencyGraph::new(),
            bindings: BindingTable::new(),
            declarations: HashMap::new(),
        }
    }

    pub fn register(&mut self, module: Module) {
        let path = module.path.clone();
        let source_path = module.source_path.clone();
        self.graph
            .add_node(ModuleNode::new(path.clone(), source_path));
        self.register_module_declarations(&module);
        let _ = self.register_module_binding(&module);
        self.modules.insert(path, module);
    }

    fn register_names<I: IntoIterator<Item = String>>(&mut self, names: I, mod_path: &ModulePath) {
        for name in names {
            self.declarations
                .entry(name)
                .or_default()
                .insert(mod_path.clone());
        }
    }

    fn register_module_declarations(&mut self, module: &Module) {
        let p = &module.path;
        self.register_names(module.program.functions.iter().map(|f| f.name.clone()), p);
        self.register_names(module.program.globals.iter().map(|g| g.name.clone()), p);
        self.register_names(module.program.structs.iter().map(|s| s.name.clone()), p);
        self.register_names(module.program.enums.iter().map(|e| e.name.clone()), p);
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

    pub fn lookup_binding(&self, name: &str) -> Option<&NameBinding> {
        self.bindings.get(name)
    }

    pub fn find_module_for_declaration(
        &self,
        name: &str,
        current_module: &ModulePath,
    ) -> Option<ModulePath> {
        let candidates = self.declarations.get(name)?;
        if candidates.contains(current_module) {
            return Some(current_module.clone());
        }
        candidates.iter().next().cloned()
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
                Some((mod_path, base_name))
            } else {
                self.find_module_for_declaration(name, current_module)
                    .map(|m| (m, name.to_string()))
            }
        }
    }

    pub fn finalize_graph(&mut self) -> CompileResult<()> {
        let paths: Vec<ModulePath> = self.modules.keys().cloned().collect();
        for path in &paths {
            if let Some(module) = self.modules.get(path) {
                for import in &module.imports {
                    if self.modules.contains_key(&import.path) {
                        self.graph.add_edge(
                            path.clone(),
                            import.path.clone(),
                            import.import_type.clone(),
                        );
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

    pub fn all_modules(&self) -> Vec<&Module> {
        self.modules.values().collect()
    }

    pub fn into_modules(self) -> Vec<Module> {
        self.modules.into_values().collect()
    }

    pub fn module_count(&self) -> usize {
        self.modules.len()
    }

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
