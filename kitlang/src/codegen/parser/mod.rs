mod expr;

use pest::iterators::Pair;

use crate::error::CompilationError;
use crate::{Rule, parse_error};

use super::ast::{Block, Function, GlobalDecl, Include, Param, Stmt};
use super::module::{ImportType, ModuleImport, ModulePath};
use super::type_ast::{
    EnumDefinition, EnumVariant, Field, ImplDefinition, RuleDecl, RuleSet, StructDefinition,
    TraitDefinition, TypeDef, UsingClause,
};
use super::types::{Type, TypeId};
use crate::error::CompileResult;

#[derive(Clone, Copy, Default, Debug)]
pub struct Parser;

impl Parser {
    pub fn new() -> Self {
        Self
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
    fn is_const_var_decl(pair: Pair<'_, Rule>) -> bool {
        pair.clone()
            .into_inner()
            .any(|p| p.as_rule() == Rule::const_kw)
    }

    /// Parse an `include` rule into an `Include`.
    pub fn parse_include(&self, pair: Pair<Rule>) -> Include {
        let mut inner = pair.into_inner();
        let path_literal_pair = inner.next().unwrap();
        let path_str = path_literal_pair.as_str();
        let path = path_str[1..path_str.len() - 1].to_string();

        let linked_lib = inner.next().map(|lib_pair| {
            let lib_str = Self::pair_text(lib_pair);
            lib_str[1..lib_str.len() - 1].to_string()
        });

        match linked_lib {
            Some(lib) => Include::with_lib(path, lib),
            None => Include::new(path),
        }
    }

    /// Parse an `import` rule into a `ModuleImport`, detecting single/wildcard/double-wildcard.
    pub fn parse_import(&self, pair: Pair<Rule>) -> ModuleImport {
        let span = pair.as_span();
        let start = span.start();
        let end = span.end();

        let mut inner = pair.into_inner();
        let import_path_pair = inner.next().unwrap();
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
        ModuleImport::with_span(path, import_type, (start, end))
    }

    /// Parse a `function_decl` rule into a `Function`.
    pub fn parse_function(&self, pair: Pair<Rule>) -> CompileResult<Function> {
        let mut inner = pair.into_inner();

        // Extract is_public from metadata_and_modifiers, if present
        let is_public = match inner.peek() {
            Some(p) if p.as_rule() == Rule::metadata_and_modifiers => {
                let modifiers = inner.next().unwrap(); // safe: peeked above
                !modifiers
                    .into_inner()
                    .any(|c| c.as_rule() == Rule::modifier && c.as_str() == "private")
            }
            _ => true,
        };

        // Function name is always next
        let name =
            Self::pair_text(inner.next().ok_or_else(|| {
                CompilationError::ParseError("function missing name".to_string())
            })?);

        let mut params: Vec<Param> = Vec::new();
        let mut return_type: Option<Type> = None;
        let mut body = Block { stmts: Vec::new() };

        for node in inner {
            match node.as_rule() {
                Rule::params => params = self.parse_params(node)?,
                Rule::type_annotation => return_type = Some(self.parse_type(node)?),
                Rule::block => body = self.parse_block(node)?,
                _ => {}
            }
        }

        Ok(Function {
            name,
            params,
            return_type,
            inferred_return: None,
            body,
            is_public,
        })
    }

    /// Parse a `struct_def` rule into a `StructDefinition`.
    pub fn parse_struct_def(&self, pair: Pair<Rule>) -> CompileResult<StructDefinition> {
        // struct_def = { "struct" ~ identifier ~ type_params? ~ "{" ~ (var_decl)* ~ "}" }
        let mut inner = pair.into_inner();

        // First child should be the struct name (identifier)
        // The "struct" keyword is consumed when matching the rule itself
        let name = Self::pair_text(
            inner
                .next()
                .filter(|p| p.as_rule() == Rule::identifier)
                .ok_or(parse_error!("struct definition missing name"))?,
        );

        // Skip type_params if present
        while let Some(peek) = inner.peek() {
            if peek.as_rule() == Rule::type_params {
                let _ = inner.next();
            } else {
                break;
            }
        }

        // Collect var_decl rules from the remaining children
        // The struct body contains var_decl elements directly (not wrapped in a block rule)
        let fields: Vec<Field> = inner
            .filter(|p| p.as_rule() == Rule::var_decl)
            .map(|p| self.parse_struct_field(p))
            .collect::<Result<_, _>>()?;

        if fields.is_empty() {
            log::warn!("Struct '{}' has empty body", name);
        }

        Ok(StructDefinition { name, fields })
    }

    /// Parse an `enum_def` rule into an `EnumDefinition`.
    pub fn parse_enum_def(&self, pair: Pair<Rule>) -> CompileResult<EnumDefinition> {
        let mut inner = pair.into_inner();

        let name = Self::pair_text(
            inner
                .next()
                .filter(|p| p.as_rule() == Rule::identifier)
                .ok_or(parse_error!("enum definition missing name"))?,
        );

        while let Some(peek) = inner.peek() {
            if peek.as_rule() == Rule::type_params {
                let _ = inner.next();
            } else {
                break;
            }
        }

        let variants: Vec<EnumVariant> = inner
            .filter(|p| p.as_rule() == Rule::enum_variant)
            .map(|p| self.parse_enum_variant(p, name.clone()))
            .collect::<Result<_, _>>()?;

        if variants.is_empty() {
            log::warn!("Enum '{}' has empty body", name);
        }

        Ok(EnumDefinition { name, variants })
    }

    fn parse_enum_variant(
        &self,
        pair: Pair<Rule>,
        parent_name: String,
    ) -> CompileResult<EnumVariant> {
        let mut identifier_found = None;
        let mut args = Vec::new();
        let mut variant_default = None;

        for child in pair.clone().into_inner() {
            match child.as_rule() {
                Rule::identifier => {
                    identifier_found = Some(Self::pair_text(child));
                }
                Rule::param => {
                    let field = self.parse_param_field(child)?;
                    args.push(field);
                }
                Rule::expr => {
                    variant_default = Some(self.parse_expr(child)?);
                }
                Rule::metadata_and_modifiers => {
                    // Skip - we already checked this
                }
                other => {
                    log::debug!("Unknown rule in enum_variant: {:?}", other);
                }
            }
        }

        let name = identifier_found.ok_or(parse_error!("enum variant missing name"))?;

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
        })
    }

    /// Parse a `trait_def` rule into a `TraitDefinition`.
    pub fn parse_trait_def(&self, pair: Pair<Rule>) -> CompileResult<TraitDefinition> {
        let mut inner = pair.into_inner();
        // First child is metadata_and_modifiers - skip it for now
        if inner.peek().map(|p| p.as_rule()) == Some(Rule::metadata_and_modifiers) {
            let _ = inner.next();
        }
        let name = Self::pair_text(
            inner
                .next()
                .filter(|p| p.as_rule() == Rule::identifier)
                .ok_or(parse_error!("trait definition missing name"))?,
        );
        // Skip type_params and trait params for now
        while inner.peek().is_some()
            && matches!(
                inner.peek().map(|p| p.as_rule()),
                Some(Rule::type_params | Rule::identifier)
            )
        {
            let _ = inner.next();
        }
        Ok(TraitDefinition {
            name,
            params: Vec::new(),
            methods: Vec::new(),
            fields: Vec::new(),
            is_public: true,
        })
    }

    /// Parse a `trait_impl` rule into an `ImplDefinition`.
    pub fn parse_trait_impl(&self, pair: Pair<Rule>) -> CompileResult<ImplDefinition> {
        let mut inner = pair.into_inner();
        // Skip metadata_and_modifiers
        if inner.peek().map(|p| p.as_rule()) == Some(Rule::metadata_and_modifiers) {
            let _ = inner.next();
        }
        let trait_type = self.parse_type(
            inner
                .next()
                .ok_or(parse_error!("trait impl missing trait type"))?,
        )?;
        // Skip type_params
        while inner.peek().is_some() && inner.peek().unwrap().as_rule() == Rule::type_params {
            let _ = inner.next();
        }
        // Skip "for" keyword - not a named rule, consumed implicitly
        let for_type = self.parse_type(
            inner
                .next()
                .ok_or(parse_error!("trait impl missing 'for' type"))?,
        )?;
        // For now, return a simple placeholder
        Ok(ImplDefinition {
            name: String::new(),
            trait_type,
            for_type,
            params: Vec::new(),
            methods: Vec::new(),
        })
    }

    /// Parse a `rule_set` rule into a `RuleSet`.
    pub fn parse_rule_set(&self, pair: Pair<Rule>) -> CompileResult<RuleSet> {
        let mut inner = pair.into_inner();
        let name = Self::pair_text(
            inner
                .next()
                .filter(|p| p.as_rule() == Rule::identifier)
                .ok_or(parse_error!("rule set missing name"))?,
        );
        let rules: Vec<RuleDecl> = inner
            .filter(|p| p.as_rule() == Rule::rule_decl)
            .map(|p| self.parse_rule_decl(p))
            .collect::<Result<_, _>>()?;
        Ok(RuleSet { name, rules })
    }

    /// Parse a `rule_decl` rule into a `RuleDecl`.
    fn parse_rule_decl(&self, pair: Pair<Rule>) -> CompileResult<RuleDecl> {
        let mut inner = pair.into_inner();
        let pattern = self.parse_expr(inner.next().ok_or(parse_error!("rule missing pattern"))?)?;
        let body = inner.next().map(|p| self.parse_expr(p)).transpose()?;
        Ok(RuleDecl { pattern, body })
    }

    /// Parse a `typedef_stmt` rule into a `TypeDef`.
    pub fn parse_typedef(&self, pair: Pair<Rule>) -> CompileResult<TypeDef> {
        let mut inner = pair.into_inner();
        // typedef_stmt = { "typedef" ~ identifier ~ "=" ~ type_annotation ~ ";" }
        let name = Self::pair_text(inner.next().unwrap());
        let type_pair = inner.next().unwrap();
        let type_def = self.parse_type(type_pair)?;
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
        // using_clause = { ("rules" ~ type_annotation) | ("implicit" ~ expr) }
        // The first alternative yields a `type_annotation` child, the second yields an `expr` child.
        let mut inner = pair.into_inner();
        let child = inner.next().unwrap();
        if child.as_rule() == Rule::type_annotation {
            Ok(UsingClause::RuleSet(self.parse_type(child)?))
        } else {
            Ok(UsingClause::Implicit(self.parse_expr(child)?))
        }
    }

    fn parse_struct_field(&self, pair: Pair<Rule>) -> CompileResult<Field> {
        // var_decl = { (var_kw | const_kw) ~ identifier ~ (":" ~ type_annotation)? ~ ("=" ~ expr)? ~ ";" }
        let name = Self::extract_first_identifier(pair.clone())
            .ok_or(parse_error!("struct field missing name"))?;

        let is_const = Self::is_const_var_decl(pair.clone());

        // Parse type annotation if present
        let annotation = Self::extract_type_annotation(pair.clone())
            .map(|type_pair| self.parse_type(type_pair))
            .transpose()?;

        // Parse default expression if present
        let default = Self::extract_default_expr(pair.clone())
            .map(|expr_pair| self.parse_expr(expr_pair))
            .transpose()?;

        Ok(Field {
            name,
            ty: TypeId::default(),
            annotation,
            is_const,
            default,
        })
    }

    /// Extract the default expression from a var_decl pair
    fn extract_default_expr(pair: Pair<'_, Rule>) -> Option<Pair<'_, Rule>> {
        pair.into_inner().find(|p| p.as_rule() == Rule::expr)
    }

    /// Parse type annotation from a var_decl pair
    fn extract_type_annotation(pair: Pair<'_, Rule>) -> Option<Pair<'_, Rule>> {
        pair.into_inner()
            .find(|p| p.as_rule() == Rule::type_annotation)
    }

    fn parse_params(&self, pair: Pair<Rule>) -> CompileResult<Vec<Param>> {
        // param_list = { param ~ ("," ~ param )* }
        pair.into_inner()
            .filter(|p: &Pair<Rule>| p.as_rule() == Rule::param)
            .map(|p: Pair<Rule>| {
                let mut inner = p.into_inner();
                // SAFETY: Grammar guarantees param has identifier and type
                let name = Self::pair_text(inner.next().unwrap());
                let type_node = inner.next().unwrap();
                let ty_ann = self.parse_type(type_node)?;
                Ok(Param {
                    name,
                    annotation: Some(ty_ann),
                    ty: TypeId::default(),
                })
            })
            .collect()
    }

    fn parse_param_field(&self, pair: Pair<Rule>) -> CompileResult<Field> {
        // param = { identifier ~ ":" ~ type_annotation ~ ( "=" ~ expr )? }
        let mut inner = pair.into_inner();
        let name = Self::pair_text(inner.next().unwrap());
        let type_node = inner.next().unwrap();
        let ty_ann = self.parse_type(type_node)?;

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

    fn parse_block(&self, pair: Pair<Rule>) -> CompileResult<Block> {
        // block = { "{" ~ (statement)* ~ "}" }
        let stmts = pair
            .into_inner()
            // grammar gives us a wrapper Rule::statement
            .filter(|p: &Pair<Rule>| p.as_rule() == Rule::statement)
            .map(|stmt_pair: Pair<Rule>| {
                // SAFETY: Grammar guarantees exactly one child in statement wrapper
                let inner = stmt_pair.into_inner().next().unwrap();
                match inner.as_rule() {
                    Rule::var_decl => self.parse_var_decl(inner),
                    Rule::expr_stmt => self.parse_expr_stmt(inner),
                    Rule::return_stmt => self.parse_return(inner),
                    Rule::if_stmt => self.parse_if_stmt(inner),
                    Rule::while_stmt => self.parse_while_stmt(inner),
                    Rule::for_stmt => self.parse_for_stmt(inner),
                    Rule::break_stmt => Ok(Stmt::Break),
                    Rule::continue_stmt => Ok(Stmt::Continue),
                    other => Err(CompilationError::ParseError(format!(
                        "unexpected statement: {other:?}",
                    ))),
                }
            })
            .collect::<Result<_, _>>()?; // Collect and propagate errors
        Ok(Block { stmts })
    }

    fn parse_var_decl(&self, pair: Pair<Rule>) -> CompileResult<Stmt> {
        // var_decl = { (var_kw | const_kw) ~ identifier ~ (":" ~ type_annotation)? ~ ("=" ~ expr)? ~ ";" }
        // Note: const_kw is silently consumed (not used for var_decl statements in current implementation)

        let name = Self::extract_first_identifier(pair.clone())
            .ok_or(parse_error!("var_decl missing identifier"))?;

        // Parse type annotation if present
        let annotation = Self::extract_type_annotation(pair.clone())
            .map(|type_pair| self.parse_type(type_pair))
            .transpose()?;

        // Parse initializer expression if present
        let init = Self::extract_init_expr(pair.clone())
            .map(|expr_pair| self.parse_expr(expr_pair))
            .transpose()?;

        Ok(Stmt::VarDecl {
            name,
            annotation,
            inferred: TypeId::default(),
            init,
        })
    }

    /// Parse a top-level `var_decl` rule into a `GlobalDecl`.
    pub fn parse_global_var_decl(&self, pair: Pair<Rule>) -> CompileResult<GlobalDecl> {
        // Parse a global variable or constant declaration at module level
        let name = Self::extract_first_identifier(pair.clone())
            .ok_or(parse_error!("global var_decl missing identifier"))?;

        let is_const = Self::is_const_var_decl(pair.clone());

        // Parse type annotation if present
        let annotation = Self::extract_type_annotation(pair.clone())
            .map(|type_pair| self.parse_type(type_pair))
            .transpose()?;

        // Parse initializer expression if present
        let init = Self::extract_init_expr(pair.clone())
            .map(|expr_pair| self.parse_expr(expr_pair))
            .transpose()?;

        Ok(GlobalDecl {
            name,
            annotation,
            inferred: TypeId::default(),
            init,
            is_const,
            is_public: true,
        })
    }

    /// Extract initializer expression from a var_decl pair
    fn extract_init_expr(pair: Pair<'_, Rule>) -> Option<Pair<'_, Rule>> {
        pair.into_inner().find(|p| p.as_rule() == Rule::expr)
    }

    fn parse_type(&self, pair: Pair<Rule>) -> CompileResult<Type> {
        let inner_rule = pair.into_inner().next().unwrap(); // Get the actual type rule (base_type, pointer_type, etc.)
        match inner_rule.as_rule() {
            Rule::base_type => {
                let mut inner_base_type = inner_rule.into_inner();
                let base_name = inner_base_type.next().unwrap().as_str().trim();
                // For now, assume no complex array types directly from parsing this,
                // and defer full array parsing if needed.
                Ok(Type::from_kit(base_name))
            }
            Rule::pointer_type => {
                let inner_ptr_type = inner_rule.into_inner().next().unwrap(); // Get the type being pointed to
                let inner_ty = self.parse_type(inner_ptr_type)?;
                Ok(Type::Ptr(Box::new(inner_ty)))
            }
            // TODO: Handle other type_annotation rules like function_type, tuple_type
            _ => Err(CompilationError::ParseError(format!(
                "Unexpected rule in type_annotation: {:?}",
                inner_rule.as_rule()
            ))),
        }
    }

    fn parse_expr_stmt(&self, pair: Pair<Rule>) -> CompileResult<Stmt> {
        // expr_stmt = { expr ~ ";" }
        // SAFETY: Grammar guarantees expression exists as first child
        let expr_pair = pair.into_inner().next().unwrap();
        Ok(Stmt::Expr(self.parse_expr(expr_pair)?))
    }

    fn parse_return(&self, pair: Pair<Rule>) -> CompileResult<Stmt> {
        // return_stmt = { "return" ~ expr? ~ ";" }
        let mut inner = pair.into_inner();
        let expr = inner.next().map(|p| self.parse_expr(p)).transpose()?;
        Ok(Stmt::Return(expr))
    }

    fn parse_if_stmt(&self, pair: Pair<Rule>) -> CompileResult<Stmt> {
        // if_stmt = { "if" ~ expr ~ block ~ else_part? }
        // else_part = { "else" ~ (block | if_stmt) }
        let mut inner = pair.into_inner();
        let cond = self.parse_expr(inner.next().unwrap())?;
        let then_branch = self.parse_block(inner.next().unwrap())?;

        let mut else_branch = None;
        if let Some(else_pair) = inner.next() {
            debug_assert_eq!(else_pair.as_rule(), Rule::else_part);
            let else_content = else_pair.into_inner().next().unwrap();
            let else_block = match else_content.as_rule() {
                Rule::block => self.parse_block(else_content)?,
                Rule::if_stmt => {
                    let if_stmt = self.parse_if_stmt(else_content)?;
                    Block {
                        stmts: vec![if_stmt],
                    }
                }
                _ => unreachable!(),
            };
            else_branch = Some(else_block);
        }

        Ok(Stmt::If {
            cond,
            then_branch,
            else_branch,
        })
    }

    fn parse_while_stmt(&self, pair: Pair<Rule>) -> CompileResult<Stmt> {
        // while_stmt = { "while" ~ expr ~ block }
        let mut inner = pair.into_inner();
        let cond = self.parse_expr(inner.next().unwrap())?;
        let body = self.parse_block(inner.next().unwrap())?;
        Ok(Stmt::While { cond, body })
    }

    fn parse_for_stmt(&self, pair: Pair<Rule>) -> CompileResult<Stmt> {
        // for_stmt = { "for" ~ identifier ~ "in" ~ expr ~ block }
        let mut inner = pair.into_inner();
        let var = Self::pair_text(inner.next().unwrap());
        let iter = self.parse_expr(inner.next().unwrap())?;
        let body = self.parse_block(inner.next().unwrap())?;
        Ok(Stmt::For { var, iter, body })
    }
}
