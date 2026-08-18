mod binding_power;
mod diagnostics;
pub(crate) mod expr_pratt;

use pest::iterators::Pair;

use crate::error::{self, CompilationError, Span};
use crate::{Rule, parse_error};

use super::ast::{
    Block, DefaultSpecialization, Expr, ExprKind, Function, GlobalDecl, Include, Literal, MatchArm,
    MatchStmt, MetaArg, Metadata, Param, Stmt, StmtKind,
};
use super::module::{ImportType, ModuleImport, ModulePath};
use super::type_ast::{
    EnumDefinition, EnumVariant, Field, ImplDefinition, RuleDecl, RuleSet, StructDefinition,
    TraitDefinition, TypeDef, TypeParam, UsingClause,
};
use super::types::{Type, TypeId};
use crate::error::CompileResult;

/// Bridge between pest and the Pratt parser.
///
/// The pest-based parser walks the grammar tree and, when it encounters an `expr` rule, hands the
/// corresponding `Pair` off of to the Pratt parser via this adapter. The adapter:
///
/// 1. Pulls the source text out of the `Pair` with `as_str()`.
/// 2. Tokenizes it with the Logos lexer.
/// 3. Parses the tokens with the Pratt parser.
/// 4. Converts the Pratt parser's `ExprParseError` to the public `CompilationError`
///
/// This is the *only* conversion point between the two parsers; it's also the only place the
/// public eror type is built from the parser-internal one. Future diagnostic improvements (spans,
/// severity, pretty rendering) are added at this single seam.
pub(crate) struct PestExpr<'a> {
    pair: Pair<'a, Rule>,
    /// The path of the file being parsed (used for error locations).
    file: String,
    /// The full source text (used to render error snippets).
    source: String,
}

impl<'a> PestExpr<'a> {
    /// Wrap a pest `Pair` whose rule is an expression, carrying the file path and full source text
    /// so parse errors can carry a location and snippet.
    pub(crate) fn new(pair: Pair<'a, Rule>, file: String, source: String) -> Self {
        Self { pair, file, source }
    }

    /// Parse the wrapped pair as a Kit expression.
    pub(crate) fn parse(self) -> CompileResult<Expr> {
        let text = self.pair.as_str();
        let span = Span::from_pest(&self.pair.as_span());
        let ctx = error::ErrorContext {
            file: self.file.clone(),
            source: self.source.clone(),
            span,
        };
        expr_pratt::parse_kit_expr(text, &self.source, self.pair.as_span().start())
            .map_err(|e| CompilationError::ParseError(e.to_human_message()).with_context(ctx))
    }
}

#[derive(Clone, Default, Debug)]
pub struct Parser {
    /// The path of the file currently being parsed, used in error locations.
    file: String,
    /// The full source text of the file currently being parsed. Used to attach source snippets to
    /// errors raised while parsing expressions.
    source: String,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Associate the source text (and path) of the file with the parser so that expression parse
    /// errors can carry a location and source snippet.
    pub(crate) fn with_source(mut self, file: String, source: String) -> Self {
        self.file = file;
        self.source = source;
        self
    }

    /// Build an `ErrorContext` from a raw pest span.
    fn context_from_span(&self, span: &pest::Span<'_>) -> error::ErrorContext {
        error::ErrorContext {
            file: self.file.clone(),
            source: self.source.clone(),
            span: Span::from_pest(span),
        }
    }

    /// Build an `ErrorContext` for the given pest `Pair`.
    fn context_for(&self, pair: &Pair<'_, Rule>) -> error::ErrorContext {
        self.context_from_span(&pair.as_span())
    }

    /// Extract the first identifier from a pair's children (e.g., variable name, field name)
    fn extract_first_identifier(pair: Pair<'_, Rule>) -> Option<String> {
        pair.into_inner()
            .find(|p| p.as_rule() == Rule::identifier)
            .map(Self::pair_text)
    }

    /// Extract the text content from a pest Pair.
    fn pair_text(pair: Pair<'_, Rule>) -> String {
        pair.as_str().to_string()
    }

    /// Check if a var_decl uses the 'const' keyword
    fn is_const_var_decl(pair: &Pair<'_, Rule>) -> bool {
        pair.clone()
            .into_inner()
            .any(|p| p.as_rule() == Rule::const_kw)
    }

    /// Parse an `include` rule into an `Include`.
    pub fn parse_include(&self, pair: Pair<Rule>) -> CompileResult<Include> {
        let parent_span = pair.as_span();
        let mut inner = pair.into_inner();
        let path_literal_pair = inner.next().ok_or_else(|| {
            parse_error!("include statement missing path")
                .with_context(self.context_from_span(&parent_span))
        })?;
        let path_str = path_literal_pair.as_str();
        let path = path_str[1..path_str.len() - 1].to_string();

        let linked_lib = inner.next().map(|lib_pair| {
            let lib_str = Self::pair_text(lib_pair);
            lib_str[1..lib_str.len() - 1].to_string()
        });

        match linked_lib {
            Some(lib) => Ok(Include::with_lib(path, lib)),
            None => Ok(Include::new(path)),
        }
    }

    /// Parse an `import` rule into a `ModuleImport`, detecting single/wildcard/double-wildcard.
    pub fn parse_import(&self, pair: Pair<Rule>) -> CompileResult<ModuleImport> {
        let parent_span = pair.as_span();
        let span = pair.as_span();
        let start = span.start();
        let end = span.end();

        let mut inner = pair.into_inner();
        let import_path_pair = inner.next().ok_or_else(|| {
            parse_error!("import statement missing path")
                .with_context(self.context_from_span(&parent_span))
        })?;
        let full_path_str = import_path_pair.as_str();

        let has_wildcard = full_path_str.ends_with(".*");
        let has_double_wildcard = full_path_str.ends_with(".**");

        let (path_str, import_type) = if has_double_wildcard {
            let trimmed = full_path_str.trim_end_matches(".**");
            (trimmed.to_string(), ImportType::DoubleWildcard)
        } else if has_wildcard {
            let trimmed = full_path_str.trim_end_matches(".*");
            (trimmed.to_string(), ImportType::Wildcard)
        } else {
            (full_path_str.to_string(), ImportType::Single)
        };

        let path = ModulePath(path_str.split('.').map(String::from).collect());
        Ok(ModuleImport::with_span(path, import_type, (start, end)))
    }

    /// Parse a `metadata_and_modifiers` pair into (metadata list, is_public).
    pub(super) fn parse_metadata_and_modifiers(
        pair: Option<Pair<'_, Rule>>,
    ) -> (Vec<Metadata>, bool) {
        let Some(p) = pair else {
            return (vec![], true);
        };
        let mut metadata = Vec::new();
        let mut is_public = true;
        for child in p.into_inner() {
            match child.as_rule() {
                Rule::metadata => {
                    if let Ok(m) = Self::parse_metadata(child) {
                        metadata.push(m);
                    }
                }
                Rule::modifier if child.as_str() == "private" => {
                    is_public = false;
                }
                _ => {}
            }
        }
        (metadata, is_public)
    }

    /// Parse a single `metadata` pair into a `Metadata`.
    fn parse_metadata(pair: Pair<'_, Rule>) -> CompileResult<Metadata> {
        let mut inner = pair.into_inner();
        let name = Self::pair_text(inner.next().ok_or_else(|| {
            CompilationError::ParseError("metadata missing identifier".to_string())
        })?);
        let mut args = Vec::new();
        for child in inner {
            let text = Self::pair_text(child);
            // Simple heuristic: quoted strings are literals, unquoted are identifiers
            if text.starts_with('"') && text.ends_with('"') {
                let val = &text[1..text.len() - 1];
                args.push(MetaArg::Literal(Literal::String(val.to_string())));
            } else if let Ok(n) = text.parse::<i64>() {
                args.push(MetaArg::Literal(Literal::Int(n)));
            } else {
                match text.as_str() {
                    "true" => args.push(MetaArg::Literal(Literal::Bool(true))),
                    "false" => args.push(MetaArg::Literal(Literal::Bool(false))),
                    "null" => args.push(MetaArg::Literal(Literal::Null)),
                    _ => args.push(MetaArg::Identifier(text)),
                }
            }
        }
        Ok(Metadata { name, args })
    }

    /// Parse an expression via the Pratt parser. This is the unified
    /// entry point used by every pest-side call site that needs an
    /// expression AST node.
    pub fn parse_expr(&self, pair: Pair<Rule>) -> CompileResult<Expr> {
        PestExpr::new(pair, self.file.clone(), self.source.clone()).parse()
    }

    pub fn parse_function(
        &self,
        pair: Pair<Rule>,
        outer_type_params: &[&str],
    ) -> CompileResult<Function> {
        let parent_span = pair.as_span();
        let mut inner = pair.into_inner();

        // Parse metadata_and_modifiers, if present
        let (metadata, is_public) = match inner.peek() {
            Some(p) if p.as_rule() == Rule::metadata_and_modifiers => {
                Self::parse_metadata_and_modifiers(inner.next())
            }
            _ => (vec![], true),
        };

        // Function name is always next
        let name = Self::pair_text(inner.next().ok_or_else(|| {
            CompilationError::ParseError("function missing name".to_string())
                .with_context(self.context_from_span(&parent_span))
        })?);

        let mut type_params: Vec<TypeParam> = Vec::new();
        let mut params: Vec<Param> = Vec::new();
        let mut return_type: Option<Type> = None;
        let mut body = Block { stmts: Vec::new() };

        for node in inner {
            match node.as_rule() {
                Rule::type_params => type_params = self.parse_type_params(node)?,
                Rule::params => {
                    let scope = Self::type_param_scope(outer_type_params, &type_params);
                    params = self.parse_params(node, &scope)?;
                }
                Rule::type_annotation => {
                    let scope = Self::type_param_scope(outer_type_params, &type_params);
                    return_type = Some(self.parse_type(node, &scope)?);
                }
                Rule::block => {
                    let scope = Self::type_param_scope(outer_type_params, &type_params);
                    body = self.parse_block(node, &scope)?;
                }
                _ => {}
            }
        }

        Ok(Function {
            name,
            type_params,
            params,
            return_type,
            inferred_return: None,
            body,
            is_public,
            metadata,
        })
    }

    /// Parse a `struct_def` rule into a `StructDefinition`.
    pub fn parse_struct_def(
        &self,
        pair: Pair<Rule>,
        metadata: Vec<Metadata>,
        is_public: bool,
    ) -> CompileResult<StructDefinition> {
        // struct_def = { "struct" ~ identifier ~ type_params? ~ "{" ~ (var_decl)* ~ "}" }
        let parent_span = pair.as_span();
        let mut inner = pair.into_inner();

        // First child should be the struct name (identifier)
        let name = Self::pair_text(
            inner
                .next()
                .filter(|p| p.as_rule() == Rule::identifier)
                .ok_or_else(|| {
                    parse_error!("struct definition missing name")
                        .with_context(self.context_from_span(&parent_span))
                })?,
        );

        // Collect type_params if present
        let mut type_params: Vec<TypeParam> = Vec::new();
        while let Some(peek) = inner.peek() {
            if peek.as_rule() == Rule::type_params {
                type_params = self.parse_type_params(inner.next().expect("peeked type_params"))?;
            } else {
                break;
            }
        }

        let names = Self::type_param_names(&type_params);
        // Collect var_decl rules from the remaining children
        let fields: Vec<Field> = inner
            .filter(|p| p.as_rule() == Rule::var_decl)
            .map(|p| self.parse_struct_field(&p, &names))
            .collect::<Result<_, _>>()?;

        if fields.is_empty() {
            log::warn!("Struct '{}' has empty body", name);
        }

        Ok(StructDefinition {
            name,
            type_params,
            fields,
            is_public,
            metadata,
        })
    }

    /// Parse an `enum_def` rule into an `EnumDefinition`.
    pub fn parse_enum_def(
        &self,
        pair: Pair<Rule>,
        metadata: Vec<Metadata>,
        is_public: bool,
    ) -> CompileResult<EnumDefinition> {
        let parent_span = pair.as_span();
        let mut inner = pair.into_inner();

        let name = Self::pair_text(
            inner
                .next()
                .filter(|p| p.as_rule() == Rule::identifier)
                .ok_or_else(|| {
                    parse_error!("enum definition missing name")
                        .with_context(self.context_from_span(&parent_span))
                })?,
        );

        // Collect type_params if present
        let mut type_params: Vec<TypeParam> = Vec::new();
        while let Some(peek) = inner.peek() {
            if peek.as_rule() == Rule::type_params {
                type_params = self.parse_type_params(inner.next().expect("peeked type_params"))?;
            } else {
                break;
            }
        }

        let names = Self::type_param_names(&type_params);
        let variants: Vec<EnumVariant> = inner
            .filter(|p| p.as_rule() == Rule::enum_variant)
            .map(|p| self.parse_enum_variant(p, name.clone(), &names))
            .collect::<Result<_, _>>()?;

        if variants.is_empty() {
            log::warn!("Enum '{}' has empty body", name);
        }

        Ok(EnumDefinition {
            name,
            type_params,
            variants,
            is_public,
            metadata,
        })
    }

    fn parse_enum_variant(
        &self,
        pair: Pair<Rule>,
        parent_name: String,
        type_params: &[&str],
    ) -> CompileResult<EnumVariant> {
        let span = pair.as_span();
        let mut inner = pair.into_inner();
        let (metadata, _is_public) = Self::parse_metadata_and_modifiers(inner.next());

        let mut identifier_found = None;
        let mut args = Vec::new();
        let mut variant_default = None;

        for child in inner {
            match child.as_rule() {
                Rule::identifier => {
                    identifier_found = Some(Self::pair_text(child));
                }
                Rule::param => {
                    let field = self.parse_param_field(child, type_params)?;
                    args.push(field);
                }
                Rule::expr => {
                    variant_default = Some(self.parse_expr(child)?);
                }
                other => {
                    log::debug!("Unknown rule in enum_variant: {:?}", other);
                }
            }
        }

        let name = identifier_found.ok_or_else(|| {
            parse_error!("enum variant missing name").with_context(self.context_from_span(&span))
        })?;

        // If there's a variant-level default, apply it to the last argument
        if let Some(default_expr) = variant_default
            && let Some(last_arg) = args.last_mut()
        {
            last_arg.default = Some(default_expr);
        }

        Ok(EnumVariant {
            name,
            parent: parent_name,
            args,
            default: None,
            metadata,
        })
    }

    /// Parse a `trait_def` rule into a `TraitDefinition`.
    pub fn parse_trait_def(&self, pair: Pair<Rule>) -> CompileResult<TraitDefinition> {
        let parent_span = pair.as_span();
        let mut inner = pair.into_inner();
        let (_metadata, is_public) = match inner.peek() {
            Some(p) if p.as_rule() == Rule::metadata_and_modifiers => {
                Self::parse_metadata_and_modifiers(inner.next())
            }
            _ => (vec![], true),
        };
        let name = Self::pair_text(
            inner
                .next()
                .filter(|p| p.as_rule() == Rule::identifier)
                .ok_or_else(|| {
                    parse_error!("trait definition missing name")
                        .with_context(self.context_from_span(&parent_span))
                })?,
        );
        // `params` holds the bracket `[T, U]` type params; `assoc_params` the
        // paren `(AssocT)` associated-type params. Both are kept in `params` so
        // no declared type parameter is lost (kitc's `TraitDefinition` has no
        // dedicated associated-types field).
        let mut params: Vec<TypeParam> = Vec::new();
        let mut assoc_params: Vec<TypeParam> = Vec::new();
        let mut methods: Vec<Function> = Vec::new();
        let mut fields: Vec<GlobalDecl> = Vec::new();
        for node in inner {
            match node.as_rule() {
                Rule::type_params => params = self.parse_type_params(node)?,
                Rule::type_param => assoc_params.push(self.parse_type_param(node)?),
                Rule::function_decl => {
                    let trait_params = Self::type_param_names(&params);
                    let scope = Self::type_param_scope(&trait_params, &assoc_params);
                    methods.push(self.parse_function(node, &scope)?);
                }
                Rule::var_decl => fields.push(self.parse_global_var_decl(&node)?),
                _ => {}
            }
        }
        params.extend(assoc_params);
        Ok(TraitDefinition {
            name,
            params,
            methods,
            fields,
            is_public,
        })
    }

    /// Parse a `trait_impl` rule into an `ImplDefinition`.
    pub fn parse_trait_impl(&self, pair: Pair<Rule>) -> CompileResult<ImplDefinition> {
        let parent_span = pair.as_span();
        let mut inner = pair.into_inner();
        // Skip metadata_and_modifiers
        if inner.peek().map(|p| p.as_rule()) == Some(Rule::metadata_and_modifiers) {
            let _ = inner.next();
        }
        let trait_type = self.parse_type(
            inner.next().ok_or_else(|| {
                parse_error!("trait impl missing trait type")
                    .with_context(self.context_from_span(&parent_span))
            })?,
            &[],
        )?;
        let mut params: Vec<TypeParam> = Vec::new();
        let mut for_type = Type::Void;
        let mut methods: Vec<Function> = Vec::new();
        for node in inner {
            match node.as_rule() {
                Rule::type_params => params = self.parse_type_params(node)?,
                Rule::type_annotation => {
                    let names = Self::type_param_names(&params);
                    for_type = self.parse_type(node, &names)?;
                }
                Rule::function_decl => {
                    let names = Self::type_param_names(&params);
                    methods.push(self.parse_function(node, &names)?);
                }
                _ => {}
            }
        }
        Ok(ImplDefinition {
            name: String::new(),
            trait_type,
            for_type,
            params,
            methods,
        })
    }

    /// Parse a `default Trait as Type;` specialization declaration into a
    /// `DefaultSpecialization`. The trait is reduced to its base name; the default type is kept
    /// as a full `Type` so it can be bound to the constrained type variable.
    pub fn parse_default_decl(&self, pair: Pair<Rule>) -> CompileResult<DefaultSpecialization> {
        let parent_span = pair.as_span();
        let mut inner = pair.into_inner();
        let trait_type = self.parse_type(
            inner.next().ok_or_else(|| {
                parse_error!("default specialization missing trait")
                    .with_context(self.context_from_span(&parent_span))
            })?,
            &[],
        )?;
        let default_type = self.parse_type(
            inner.next().ok_or_else(|| {
                parse_error!("default specialization missing default type")
                    .with_context(self.context_from_span(&parent_span))
            })?,
            &[],
        )?;
        let trait_name = match &trait_type {
            Type::Named(name) => name.clone(),
            Type::Instance { base, .. } => base.clone(),
            other => {
                return Err(parse_error!(
                    "default specialization trait must be a trait name, got {:?}",
                    other
                )
                .with_context(self.context_from_span(&parent_span)));
            }
        };
        Ok(DefaultSpecialization {
            trait_name,
            default_type,
        })
    }

    /// Parse a `rule_set` rule into a `RuleSet`.
    pub fn parse_rule_set(&self, pair: Pair<Rule>) -> CompileResult<RuleSet> {
        let parent_span = pair.as_span();
        let mut inner = pair.into_inner();
        let name = Self::pair_text(
            inner
                .next()
                .filter(|p| p.as_rule() == Rule::identifier)
                .ok_or_else(|| {
                    parse_error!("rule set missing name")
                        .with_context(self.context_from_span(&parent_span))
                })?,
        );
        let rules: Vec<RuleDecl> = inner
            .filter(|p| p.as_rule() == Rule::rule_decl)
            .map(|p| self.parse_rule_decl(p))
            .collect::<Result<_, _>>()?;
        Ok(RuleSet { name, rules })
    }

    /// Parse a `rule_decl` rule into a `RuleDecl`.
    fn parse_rule_decl(&self, pair: Pair<Rule>) -> CompileResult<RuleDecl> {
        let parent_span = pair.as_span();
        let mut inner = pair.into_inner();
        let pattern = self.parse_expr(inner.next().ok_or_else(|| {
            parse_error!("rule missing pattern").with_context(self.context_from_span(&parent_span))
        })?)?;
        let body = inner.next().map(|p| self.parse_expr(p)).transpose()?;
        Ok(RuleDecl { pattern, body })
    }

    /// Parse a `typedef_stmt` rule into a `TypeDef`.
    pub fn parse_typedef(&self, pair: Pair<Rule>) -> CompileResult<TypeDef> {
        let parent_span = pair.as_span();
        let mut inner = pair.into_inner();
        // typedef_stmt = { "typedef" ~ identifier ~ "=" ~ type_annotation ~ ";" }
        let name = Self::pair_text(inner.next().ok_or_else(|| {
            parse_error!("typedef missing name").with_context(self.context_from_span(&parent_span))
        })?);
        let type_pair = inner.next().ok_or_else(|| {
            parse_error!("typedef missing type").with_context(self.context_from_span(&parent_span))
        })?;
        let type_def = self.parse_type(type_pair, &[])?;
        Ok(TypeDef { name, type_def })
    }

    /// Parse a `using_stmt` rule into a `Vec<UsingClause>`.
    pub fn parse_using(&self, pair: Pair<Rule>) -> CompileResult<Vec<UsingClause>> {
        // using_stmt = { "using" ~ (using_clause ~ ("," ~ using_clause)*) ~ ";" }
        let clauses: CompileResult<Vec<_>> = pair
            .into_inner()
            .filter(|p| p.as_rule() == Rule::using_clause)
            .map(|p| self.parse_using_clause(p))
            .collect();
        clauses
    }

    /// Parse a single `using_clause` rule into a `UsingClause`.
    fn parse_using_clause(&self, pair: Pair<Rule>) -> CompileResult<UsingClause> {
        let parent_span = pair.as_span();
        // using_clause = { ("rules" ~ type_annotation) | ("implicit" ~ expr) }
        // First alternative yields a `type_annotation` child, the second an `expr` child.
        let mut inner = pair.into_inner();
        let child = inner.next().ok_or_else(|| {
            parse_error!("using clause is empty").with_context(self.context_from_span(&parent_span))
        })?;
        if child.as_rule() == Rule::type_annotation {
            Ok(UsingClause::RuleSet(self.parse_type(child, &[])?))
        } else {
            Ok(UsingClause::Implicit(self.parse_expr(child)?))
        }
    }

    /// Parse a single `type_param` into a `TypeParam`.
    fn parse_type_param(&self, pair: Pair<Rule>) -> CompileResult<TypeParam> {
        let parent_span = pair.as_span();
        let mut inner = pair.into_inner();
        let name = Self::pair_text(inner.next().ok_or_else(|| {
            parse_error!("type parameter missing name")
                .with_context(self.context_from_span(&parent_span))
        })?);
        let mut constraints = Vec::new();
        let mut default = None;
        for child in inner {
            match child.as_rule() {
                Rule::type_constraints => {
                    for constraint in child.into_inner() {
                        if constraint.as_rule() == Rule::type_annotation {
                            constraints.push(self.parse_type(constraint, &[])?);
                        }
                    }
                }
                Rule::type_annotation => default = Some(self.parse_type(child, &[])?),
                _ => {}
            }
        }
        Ok(TypeParam {
            name,
            constraints,
            default,
        })
    }

    /// Parse a `type_params` rule into a list of `TypeParam`s.
    fn parse_type_params(&self, pair: Pair<Rule>) -> CompileResult<Vec<TypeParam>> {
        pair.into_inner()
            .filter(|p| p.as_rule() == Rule::type_param)
            .map(|p| self.parse_type_param(p))
            .collect()
    }

    /// Borrow the names of a list of type parameters.
    fn type_param_names(params: &[TypeParam]) -> Vec<&str> {
        params.iter().map(|tp| tp.name.as_str()).collect()
    }

    /// Combine enclosing (outer) type-parameter names with a definition's own.
    fn type_param_scope<'a>(outer: &'a [&'a str], params: &'a [TypeParam]) -> Vec<&'a str> {
        let mut scope = outer.to_vec();
        scope.extend(Self::type_param_names(params));
        scope
    }

    fn parse_struct_field(&self, pair: &Pair<Rule>, type_params: &[&str]) -> CompileResult<Field> {
        // var_decl = { (var_kw | const_kw) ~ identifier
        //   ~ (":" ~ type_annotation)? ~ ("=" ~ expr)? ~ ";" }
        let name = Self::extract_first_identifier(pair.clone()).ok_or_else(|| {
            parse_error!("struct field missing name").with_context(self.context_for(pair))
        })?;

        let is_const = Self::is_const_var_decl(pair);

        let annotation = Self::extract_first_rule(pair.clone(), Rule::type_annotation)
            .map(|p| self.parse_type(p, type_params))
            .transpose()?;

        let default = Self::extract_first_rule(pair.clone(), Rule::expr)
            .map(|p| self.parse_expr(p))
            .transpose()?;

        if annotation.is_none() && default.is_none() {
            return Err(parse_error!(
                "struct field '{name}' must have a type annotation or initializer"
            )
            .with_context(self.context_for(pair)));
        }

        Ok(Field {
            name,
            ty: TypeId::default(),
            annotation,
            is_const,
            default,
        })
    }

    /// Find the first child pair matching the given rule.
    fn extract_first_rule(pair: Pair<'_, Rule>, rule: Rule) -> Option<Pair<'_, Rule>> {
        pair.into_inner().find(|p| p.as_rule() == rule)
    }

    fn parse_params(&self, pair: Pair<Rule>, type_params: &[&str]) -> CompileResult<Vec<Param>> {
        let parent_span = pair.as_span();
        // param_list = { param ~ ("," ~ param )* }
        pair.into_inner()
            .filter(|p: &Pair<Rule>| p.as_rule() == Rule::param)
            .map(|p: Pair<Rule>| {
                let mut inner = p.into_inner();
                let name = Self::pair_text(inner.next().ok_or_else(|| {
                    parse_error!("param missing name")
                        .with_context(self.context_from_span(&parent_span))
                })?);
                let type_node = inner.next().ok_or_else(|| {
                    parse_error!("param missing type")
                        .with_context(self.context_from_span(&parent_span))
                })?;
                let ty_ann = self.parse_type(type_node, type_params)?;
                Ok(Param {
                    name,
                    annotation: Some(ty_ann),
                    ty: TypeId::default(),
                })
            })
            .collect()
    }

    fn parse_param_field(&self, pair: Pair<Rule>, type_params: &[&str]) -> CompileResult<Field> {
        let parent_span = pair.as_span();
        // param = { identifier ~ ":" ~ type_annotation ~ ( "=" ~ expr )? }
        let mut inner = pair.into_inner();
        let name = Self::pair_text(inner.next().ok_or_else(|| {
            parse_error!("param field missing name")
                .with_context(self.context_from_span(&parent_span))
        })?);
        let type_node = inner.next().ok_or_else(|| {
            parse_error!("param field missing type annotation")
                .with_context(self.context_from_span(&parent_span))
        })?;
        let ty_ann = self.parse_type(type_node, type_params)?;

        // Check for optional default expression
        let default = inner
            .next()
            .map(|expr_pair| self.parse_expr(expr_pair))
            .transpose()?;

        Ok(Field {
            name,
            ty: TypeId::default(),
            annotation: Some(ty_ann),
            is_const: false,
            default,
        })
    }

    fn parse_block(&self, pair: Pair<Rule>, type_params: &[&str]) -> CompileResult<Block> {
        let parent_span = pair.as_span();
        // block = { "{" ~ (statement)* ~ "}" }
        let stmts = pair
            .into_inner()
            // grammar gives us a wrapper Rule::statement
            .filter(|p: &Pair<Rule>| p.as_rule() == Rule::statement)
            .map(|stmt_pair: Pair<Rule>| {
                self.parse_statement_pair(&stmt_pair, &parent_span, type_params)
            })
            .collect::<Result<_, _>>()?;
        Ok(Block { stmts })
    }

    /// Parse a single `Rule::statement` wrapper into a `Stmt`.
    fn parse_statement_pair(
        &self,
        stmt_pair: &Pair<Rule>,
        parent_span: &pest::Span<'_>,
        type_params: &[&str],
    ) -> CompileResult<Stmt> {
        let inner = stmt_pair.clone().into_inner().next().ok_or_else(|| {
            parse_error!("statement wrapper is empty")
                .with_context(self.context_from_span(parent_span))
        })?;
        match inner.as_rule() {
            Rule::defer_stmt => self.parse_defer(inner, type_params),
            Rule::block_stmt => {
                // The grammar wraps the inner `block` rule inside a `block_stmt` wrapper,
                // which follows the same pattern as `defer_stmt` wrapping a `statement`.
                //
                // Unwrap it before delegating to `parse_block`, which expects a `Rule::body` pair
                // containing `Rule::statement` children.
                let block_pair = inner.clone().into_inner().next().ok_or_else(|| {
                    parse_error!("block_stmt wrapper is empty")
                        .with_context(self.context_from_span(parent_span))
                })?;

                debug_assert_eq!(block_pair.as_rule(), Rule::block);

                let block = self.parse_block(block_pair, type_params)?;

                Ok(Stmt {
                    kind: StmtKind::Block(block),
                    span: Span::from_pest(&inner.as_span()),
                })
            }
            Rule::var_decl => self.parse_var_decl(&inner, type_params),
            Rule::expr_stmt => self.parse_expr_stmt(inner),
            Rule::return_stmt => self.parse_return(inner),
            Rule::if_stmt => self.parse_if_stmt(inner, type_params),
            Rule::while_stmt => self.parse_while_stmt(inner, type_params),
            Rule::for_stmt => self.parse_for_stmt(inner, type_params),
            Rule::break_stmt => Ok(Stmt {
                kind: StmtKind::Break,
                span: Span::from_pest(&inner.as_span()),
            }),
            Rule::continue_stmt => Ok(Stmt {
                kind: StmtKind::Continue,
                span: Span::from_pest(&inner.as_span()),
            }),
            Rule::match_stmt => self.parse_match_stmt(inner),
            other => Err(CompilationError::ParseError(format!(
                "unexpected statement: {other:?}",
            ))),
        }
    }

    fn parse_defer(&self, pair: Pair<Rule>, type_params: &[&str]) -> CompileResult<Stmt> {
        let parent_span = pair.as_span();
        // defer_stmt = { "defer" ~ statement }
        let mut inner = pair.into_inner();
        let body_pair = inner.next().ok_or_else(|| {
            parse_error!("defer missing body").with_context(self.context_from_span(&parent_span))
        })?;
        debug_assert_eq!(body_pair.as_rule(), Rule::statement);
        let body = Box::new(self.parse_statement_pair(&body_pair, &parent_span, type_params)?);
        Ok(Stmt {
            kind: StmtKind::Defer { body },
            span: Span::from_pest(&parent_span),
        })
    }

    fn parse_var_decl(&self, pair: &Pair<Rule>, type_params: &[&str]) -> CompileResult<Stmt> {
        // var_decl = { (var_kw | const_kw) ~ identifier
        //   ~ (":" ~ type_annotation)? ~ ("=" ~ expr)? ~ ";" }
        // const_kw is accepted but not tracked; var_decl statements have no const semantics today.

        let name = Self::extract_first_identifier(pair.clone()).ok_or_else(|| {
            parse_error!("var_decl missing identifier").with_context(self.context_for(pair))
        })?;

        let annotation = Self::extract_first_rule(pair.clone(), Rule::type_annotation)
            .map(|p| self.parse_type(p, type_params))
            .transpose()?;

        let init = Self::extract_first_rule(pair.clone(), Rule::expr)
            .map(|p| self.parse_expr(p))
            .transpose()?;

        let span = Span::from_pest(&pair.as_span());
        Ok(Stmt {
            kind: StmtKind::VarDecl {
                name,
                annotation,
                inferred: TypeId::default(),
                init,
            },
            span,
        })
    }

    /// Parse a top-level `var_decl` rule into a `GlobalDecl`.
    pub fn parse_global_var_decl(&self, pair: &Pair<Rule>) -> CompileResult<GlobalDecl> {
        // Extract metadata_and_modifiers, if present
        let (metadata, is_public) = match pair
            .clone()
            .into_inner()
            .find(|p| p.as_rule() == Rule::metadata_and_modifiers)
        {
            Some(mm) => Self::parse_metadata_and_modifiers(Some(mm)),
            None => (vec![], true),
        };

        // Parse a global variable or constant declaration at module level
        let name = Self::extract_first_identifier(pair.clone()).ok_or_else(|| {
            parse_error!("global var_decl missing identifier").with_context(self.context_for(pair))
        })?;

        let is_const = Self::is_const_var_decl(pair);

        let annotation = Self::extract_first_rule(pair.clone(), Rule::type_annotation)
            .map(|p| self.parse_type(p, &[]))
            .transpose()?;

        let init = Self::extract_first_rule(pair.clone(), Rule::expr)
            .map(|p| self.parse_expr(p))
            .transpose()?;

        Ok(GlobalDecl {
            name,
            annotation,
            inferred: TypeId::default(),
            init,
            is_const,
            is_public,
            metadata,
        })
    }

    fn parse_type(&self, pair: Pair<Rule>, type_params: &[&str]) -> CompileResult<Type> {
        let parent_span = pair.as_span();
        let inner_rule = pair.into_inner().next().ok_or_else(|| {
            parse_error!("type annotation is empty")
                .with_context(self.context_from_span(&parent_span))
        })?;
        match inner_rule.as_rule() {
            Rule::tuple_type => {
                let inner_base_type = inner_rule.into_inner();
                let mut elem_types: Vec<Type> = Vec::new();
                for elem in inner_base_type {
                    elem_types.push(self.parse_type(elem, type_params)?);
                }
                Ok(Type::Tuple(elem_types))
            }
            Rule::base_type => {
                let mut inner_base_type = inner_rule.into_inner();
                let base_name = inner_base_type
                    .next()
                    .ok_or_else(|| {
                        parse_error!("base type is empty")
                            .with_context(self.context_from_span(&parent_span))
                    })?
                    .as_str()
                    .trim();
                // Remaining children (if any) are the `[...]` type arguments,
                // making this an application of a generic type (`List[Int]`).
                let mut args: Vec<Type> = Vec::new();
                for arg in inner_base_type {
                    args.push(self.parse_type(arg, type_params)?);
                }
                if !args.is_empty() {
                    Ok(Type::Instance {
                        base: base_name.to_string(),
                        args,
                    })
                } else if type_params.contains(&base_name) {
                    // A name matching an in-scope generic parameter is a bound
                    // type-parameter reference (`T` in `value: T`), not a named type.
                    Ok(Type::TypeParam(base_name.to_string()))
                } else {
                    Ok(Type::from_kit(base_name))
                }
            }
            Rule::pointer_type => {
                let inner_ptr_type = inner_rule.into_inner().next().ok_or_else(|| {
                    parse_error!("pointer type is empty")
                        .with_context(self.context_from_span(&parent_span))
                })?;
                let inner_ty = self.parse_type(inner_ptr_type, type_params)?;
                Ok(Type::Ptr(Box::new(inner_ty)))
            }
            Rule::function_type => {
                let inner = inner_rule.into_inner();
                // All type_annotation pairs from the params (zero or more),
                // followed by the return type as the last pair.
                let mut type_pairs: Vec<Pair<Rule>> = inner.collect();
                let ret_pair = type_pairs.pop().ok_or_else(|| {
                    parse_error!("function_type missing return type")
                        .with_context(self.context_from_span(&parent_span))
                })?;
                let ret_ty = self.parse_type(ret_pair, type_params)?;
                let param_tys: Result<Vec<Type>, CompilationError> = type_pairs
                    .into_iter()
                    .map(|p| self.parse_type(p, type_params))
                    .collect();
                Ok(Type::Function {
                    param_tys: param_tys?,
                    ret_ty: Box::new(ret_ty),
                })
            }
            _ => Err(CompilationError::ParseError(format!(
                "Unexpected rule in type_annotation: {:?}",
                inner_rule.as_rule()
            ))),
        }
    }

    fn parse_expr_stmt(&self, pair: Pair<Rule>) -> CompileResult<Stmt> {
        let parent_span = pair.as_span();
        let span = Span::from_pest(&parent_span);
        // expr_stmt = { expr ~ ";" }
        let expr_pair = pair.into_inner().next().ok_or_else(|| {
            parse_error!("expression statement is empty")
                .with_context(self.context_from_span(&parent_span))
        })?;
        let expr = self.parse_expr(expr_pair)?;
        Ok(Stmt {
            kind: StmtKind::Expr(expr),
            span,
        })
    }

    fn parse_return(&self, pair: Pair<Rule>) -> CompileResult<Stmt> {
        // return_stmt = { "return" ~ expr? ~ ";" }
        let span = Span::from_pest(&pair.as_span());
        let mut inner = pair.into_inner();
        let expr = inner.next().map(|p| self.parse_expr(p)).transpose()?;
        Ok(Stmt {
            kind: StmtKind::Return(expr),
            span,
        })
    }

    fn parse_if_stmt(&self, pair: Pair<Rule>, type_params: &[&str]) -> CompileResult<Stmt> {
        let parent_span = pair.as_span();
        // if_stmt = { "if" ~ expr ~ block ~ else_part? }
        // else_part = { "else" ~ (block | if_stmt) }
        let mut inner = pair.into_inner();
        let cond = self.parse_expr(inner.next().ok_or_else(|| {
            parse_error!("if statement missing condition")
                .with_context(self.context_from_span(&parent_span))
        })?)?;
        let then_branch = self.parse_block(
            inner.next().ok_or_else(|| {
                parse_error!("if statement missing then branch")
                    .with_context(self.context_from_span(&parent_span))
            })?,
            type_params,
        )?;

        let mut else_branch = None;
        if let Some(else_pair) = inner.next() {
            debug_assert_eq!(else_pair.as_rule(), Rule::else_part);
            let else_content = else_pair.into_inner().next().ok_or_else(|| {
                parse_error!("else part is empty")
                    .with_context(self.context_from_span(&parent_span))
            })?;
            let else_block = match else_content.as_rule() {
                Rule::block => self.parse_block(else_content, type_params)?,
                Rule::if_stmt => {
                    let if_stmt = self.parse_if_stmt(else_content, type_params)?;
                    Block {
                        stmts: vec![if_stmt],
                    }
                }
                _ => unreachable!(
                    "else_content rule should be block or if_stmt, got {:?}",
                    else_content.as_rule()
                ),
            };
            else_branch = Some(else_block);
        }

        Ok(Stmt {
            kind: StmtKind::If {
                cond,
                then_branch,
                else_branch,
            },
            span: Span::from_pest(&parent_span),
        })
    }

    fn parse_while_stmt(&self, pair: Pair<Rule>, type_params: &[&str]) -> CompileResult<Stmt> {
        let parent_span = pair.as_span();
        let span = Span::from_pest(&parent_span);
        // while_stmt = { "while" ~ expr ~ block }
        let mut inner = pair.into_inner();
        let cond = self.parse_expr(inner.next().ok_or_else(|| {
            parse_error!("while statement missing condition")
                .with_context(self.context_from_span(&parent_span))
        })?)?;
        let body = self.parse_block(
            inner.next().ok_or_else(|| {
                parse_error!("while statement missing body")
                    .with_context(self.context_from_span(&parent_span))
            })?,
            type_params,
        )?;
        Ok(Stmt {
            kind: StmtKind::While { cond, body },
            span,
        })
    }

    fn parse_for_stmt(&self, pair: Pair<Rule>, type_params: &[&str]) -> CompileResult<Stmt> {
        let parent_span = pair.as_span();
        let span = Span::from_pest(&parent_span);
        // for_stmt = { "for" ~ identifier ~ "in" ~ expr ~ block }
        let mut inner = pair.into_inner();
        let var = Self::pair_text(inner.next().ok_or_else(|| {
            parse_error!("for statement missing variable")
                .with_context(self.context_from_span(&parent_span))
        })?);
        let iter = self.parse_expr(inner.next().ok_or_else(|| {
            parse_error!("for statement missing iterable")
                .with_context(self.context_from_span(&parent_span))
        })?)?;
        let body = self.parse_block(
            inner.next().ok_or_else(|| {
                parse_error!("for statement missing body")
                    .with_context(self.context_from_span(&parent_span))
            })?,
            type_params,
        )?;
        Ok(Stmt {
            kind: StmtKind::For { var, iter, body },
            span,
        })
    }

    fn parse_match_stmt(&self, pair: Pair<Rule>) -> CompileResult<Stmt> {
        let parent_span = pair.as_span();
        let span = Span::from_pest(&parent_span);
        // match_stmt = { "match" ~ expr ~ "{" ~ (match_case)* ~ (default_case)? ~ "}" }
        let mut inner = pair.into_inner();
        let expr = self.parse_expr(inner.next().ok_or_else(|| {
            parse_error!("match statement missing expression")
                .with_context(self.context_from_span(&parent_span))
        })?)?;
        let mut arms = Vec::new();
        let mut default_arm = None;
        for child in inner {
            match child.as_rule() {
                Rule::match_case => {
                    let case_span = child.as_span();
                    let mut case_inner = child.into_inner();
                    let pattern = self.parse_expr(case_inner.next().ok_or_else(|| {
                        parse_error!("match case missing pattern")
                            .with_context(self.context_from_span(&case_span))
                    })?)?;
                    let body_expr = self.parse_expr(case_inner.next().ok_or_else(|| {
                        parse_error!("match case missing body")
                            .with_context(self.context_from_span(&case_span))
                    })?)?;
                    let body = Block {
                        stmts: vec![Stmt {
                            kind: StmtKind::Expr(body_expr),
                            span: Span::from_pest(&case_span),
                        }],
                    };
                    arms.push(MatchArm {
                        pattern,
                        body,
                        span: Span::from_pest(&case_span),
                    });
                }
                Rule::default_case => {
                    let def_span = child.as_span();
                    let mut def_inner = child.into_inner();
                    let body_expr = self.parse_expr(def_inner.next().ok_or_else(|| {
                        parse_error!("default case missing body")
                            .with_context(self.context_from_span(&def_span))
                    })?)?;
                    let body = Block {
                        stmts: vec![Stmt {
                            kind: StmtKind::Expr(body_expr),
                            span: Span::from_pest(&def_span),
                        }],
                    };
                    default_arm = Some(body);
                }
                _ => {}
            }
        }
        if let Some(def) = default_arm {
            let match_span = parent_span;
            arms.push(MatchArm {
                // `TypeId::default()` is a sentinel: the codegen always checks `name == "default"`
                // before touching `ty`, so this is never read.
                //
                // If you add a new code path that inspects the pattern's `ty`, guard it against
                // the sentinel first.
                pattern: Expr {
                    kind: ExprKind::Identifier {
                        name: "default".to_string(),
                    },
                    ty: TypeId::default(),
                    span: Span::from_pest(&match_span),
                },
                body: def,
                span: Span::from_pest(&match_span),
            });
        }
        Ok(Stmt {
            kind: StmtKind::Match(MatchStmt {
                expr: Box::new(expr),
                arms,
                span: Span::from_pest(&parent_span),
            }),
            span,
        })
    }
}
