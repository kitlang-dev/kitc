use pest::iterators::Pair;

use crate::codegen::types::{BinaryOperator, UnaryOperator};
use crate::error::CompilationError;
use crate::{Rule, parse_error};

use super::ast::{Block, Expr, Function, GlobalDecl, Include, Literal, Param, Stmt};
use super::type_ast::{EnumDefinition, EnumVariant, Field, FieldInit, StructDefinition};
use super::types::{AssignmentOperator, Type, TypeId};
use crate::error::CompileResult;

use std::path::PathBuf;
use std::str::FromStr;

#[derive(Default, Debug)]
pub struct Parser {
    _current_file: Option<PathBuf>,
    _source_content: String,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_file(file: PathBuf) -> Self {
        let source_content = std::fs::read_to_string(&file).unwrap_or_else(|_| String::new());

        Self {
            _current_file: Some(file),
            _source_content: source_content,
        }
    }

    /// Extract the first identifier from a pair's children (e.g., variable name, field name)
    fn extract_first_identifier(pair: Pair<'_, Rule>) -> Option<String> {
        pair.into_inner()
            .find(|p| p.as_rule() == Rule::identifier)
            .map(|p| p.as_str().to_string())
    }

    /// Check if a var_decl uses the 'const' keyword
    fn is_const_var_decl(pair: Pair<'_, Rule>) -> bool {
        pair.clone()
            .into_inner()
            .any(|p| p.as_rule() == Rule::const_kw)
    }

    pub fn parse_include(&self, pair: Pair<Rule>) -> Include {
        // include_stmt = { "include" ~ string ~ ("=>" ~ string)? ~ ";" }
        let mut inner = pair.into_inner();
        // The first child is the string literal representing the include path
        let path_literal_pair = inner.next().unwrap();
        let path_str = path_literal_pair.as_str();

        // Strip the quotes from the string literal
        let path = path_str[1..path_str.len() - 1].to_string();
        Include { path }
    }

    pub fn parse_function(&self, pair: Pair<Rule>) -> CompileResult<Function> {
        let mut inner = pair.into_inner();

        // SAFETY: Grammar guarantees function name exists as first child
        let name = inner.next().unwrap().as_str().to_string();

        let mut params: Vec<Param> = Vec::new();
        let mut return_type: Option<Type> = None;
        let mut body = Block { stmts: Vec::new() };

        // consume remaining child nodes and match on their rules
        for node in inner {
            match node.as_rule() {
                Rule::params => {
                    params = self.parse_params(node)?;
                }
                Rule::type_annotation => {
                    return_type = Some(self.parse_type(node)?);
                }
                Rule::block => {
                    body = self.parse_block(node)?;
                }
                _ => {
                    // skip punctuation/other tokens
                }
            }
        }

        Ok(Function {
            name,
            params,
            return_type,
            inferred_return: None,
            body,
        })
    }

    pub fn parse_struct_def(&self, pair: Pair<Rule>) -> CompileResult<StructDefinition> {
        // struct_def = { "struct" ~ identifier ~ type_params? ~ "{" ~ (var_decl)* ~ "}" }
        let mut inner = pair.into_inner();

        // First child should be the struct name (identifier)
        // The "struct" keyword is consumed when matching the rule itself
        let name = inner
            .next()
            .filter(|p| p.as_rule() == Rule::identifier)
            .ok_or(parse_error!("struct definition missing name"))?
            .as_str()
            .to_string();

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

    /// Parse a struct definition from a type_def rule wrapper
    pub fn parse_struct_def_from_type_def(
        &self,
        pair: Pair<Rule>,
    ) -> CompileResult<StructDefinition> {
        // Find the struct_def rule within the type_def
        let mut found_struct = None;
        for child in pair.into_inner() {
            if child.as_rule() == Rule::struct_def {
                found_struct = Some(child);
                break;
            }
        }

        let struct_def_pair =
            found_struct.ok_or(parse_error!("type_def does not contain struct_def"))?;

        self.parse_struct_def(struct_def_pair)
    }

    pub fn parse_enum_def(&self, pair: Pair<Rule>) -> CompileResult<EnumDefinition> {
        let mut inner = pair.into_inner();

        let name = inner
            .next()
            .filter(|p| p.as_rule() == Rule::identifier)
            .ok_or(parse_error!("enum definition missing name"))?
            .as_str()
            .to_string();

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

    pub fn parse_enum_def_from_type_def(&self, pair: Pair<Rule>) -> CompileResult<EnumDefinition> {
        let mut found_enum = None;
        for child in pair.into_inner() {
            if child.as_rule() == Rule::enum_def {
                found_enum = Some(child);
                break;
            }
        }

        let enum_def_pair = found_enum.ok_or(parse_error!("type_def does not contain enum_def"))?;

        self.parse_enum_def(enum_def_pair)
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
                    identifier_found = Some(child.as_str().to_string());
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
    fn extract_default_expr(pair: Pair<'_, Rule>) -> Option<pest::iterators::Pair<'_, Rule>> {
        pair.into_inner().find(|p| p.as_rule() == Rule::expr)
    }

    /// Parse type annotation from a var_decl pair
    fn extract_type_annotation(pair: Pair<'_, Rule>) -> Option<pest::iterators::Pair<'_, Rule>> {
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
                let name = inner.next().unwrap().as_str().to_string();
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
        let name = inner.next().unwrap().as_str().to_string();
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
        })
    }

    /// Extract initializer expression from a var_decl pair
    fn extract_init_expr(pair: Pair<'_, Rule>) -> Option<pest::iterators::Pair<'_, Rule>> {
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
            assert_eq!(else_pair.as_rule(), Rule::else_part);
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
        let var = inner.next().unwrap().as_str().to_string();
        let iter = self.parse_expr(inner.next().unwrap())?;
        let body = self.parse_block(inner.next().unwrap())?;
        Ok(Stmt::For { var, iter, body })
    }

    fn parse_expr(&self, pair: Pair<Rule>) -> CompileResult<Expr> {
        match pair.as_rule() {
            Rule::expr => {
                // SAFETY: Grammar guarantees exactly one child for wrapper rules
                let inner = pair.into_inner().next().unwrap();
                self.parse_expr(inner)
            }
            Rule::assign => self.parse_assign_expr(pair),
            Rule::logical_or
            | Rule::logical_and
            | Rule::equality
            | Rule::comparison
            | Rule::additive
            | Rule::multiplicative
            | Rule::bitwise_or
            | Rule::bitwise_xor
            | Rule::bitwise_and
            | Rule::shift => {
                let mut inner = pair.into_inner();
                let mut left = self.parse_expr(inner.next().unwrap())?;

                while let Some(op_pair) = inner.next() {
                    let op = BinaryOperator::from_rule_pair(&op_pair)?;
                    let right = self.parse_expr(inner.next().unwrap())?;
                    left = Expr::BinaryOp {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                        ty: TypeId::default(),
                    };
                }
                Ok(left)
            }
            Rule::unary => {
                let mut inner_pairs = pair.into_inner();
                // SAFETY: Unary rule always has at least one child
                let first_pair = inner_pairs.next().unwrap();

                match first_pair.as_rule() {
                    Rule::unary_op => {
                        let op_str = first_pair.as_str();
                        let op = UnaryOperator::from_str(op_str)
                            .map_err(|()| parse_error!("invalid unary operation: {op_str}"))?;

                        // SAFETY: Grammar guarantees expression after unary op
                        let expr = self.parse_expr(inner_pairs.next().unwrap())?;
                        Ok(Expr::UnaryOp {
                            op,
                            expr: Box::new(expr),
                            ty: TypeId::default(),
                        })
                    }
                    Rule::ADDRESS_OF_OP => {
                        // Handle address-of operator explicitly
                        let op = UnaryOperator::AddressOf;
                        let expr = self.parse_expr(inner_pairs.next().unwrap())?;
                        Ok(Expr::UnaryOp {
                            op,
                            expr: Box::new(expr),
                            ty: TypeId::default(),
                        })
                    }
                    Rule::postfix => self.parse_expr(first_pair),
                    Rule::primary => self.parse_expr(first_pair),
                    other => Err(parse_error!("Unexpected rule in unary: {other:?}")),
                }
            }
            Rule::identifier => Ok(Expr::Identifier(
                pair.as_str().to_string(),
                TypeId::default(),
            )),
            Rule::literal => {
                // SAFETY: Grammar guarantees exactly one child in literal
                let inner = pair.into_inner().next().unwrap();
                match inner.as_rule() {
                    Rule::number => {
                        // SAFETY: Number always has exactly one child (integer/float)
                        let num_pair = inner.into_inner().next().unwrap();
                        match num_pair.as_rule() {
                            Rule::integer => {
                                let s = num_pair.as_str();
                                let i = s.parse::<i64>().map_err(|e| {
                                    parse_error!("invalid integer literal '{s}': {:?}", e)
                                })?;
                                Ok(Expr::Literal(Literal::Int(i), TypeId::default()))
                            }
                            Rule::float => {
                                let s = num_pair.as_str();
                                let f = s.parse::<f64>().map_err(|e| {
                                    parse_error!("invalid float literal '{s}': {:?}", e)
                                })?;
                                Ok(Expr::Literal(Literal::Float(f), TypeId::default()))
                            }
                            _ => Err(parse_error!("Unexpected number type")),
                        }
                    }
                    Rule::boolean => Self::parse_bool_literal(inner.as_str()),
                    Rule::char_literal => todo!("char literal parsing not implemented"),
                    _ => Err(parse_error!(
                        "Unexpected literal type: {:?}",
                        inner.as_rule()
                    )),
                }
            }
            Rule::string => {
                let full = pair.as_str();
                let inner = &full[1..full.len() - 1];
                let unescaped = Self::unescape(inner).unwrap_or_else(|| inner.to_string());
                Ok(Expr::Literal(Literal::String(unescaped), TypeId::default()))
            }
            Rule::function_call_expr => {
                let mut inner = pair.into_inner();
                // SAFETY: Grammar guarantees callee identifier exists
                let callee = inner.next().unwrap().as_str().to_string();
                let args = inner
                    .filter(|p: &Pair<Rule>| p.as_rule() == Rule::expr)
                    .map(|p: Pair<Rule>| self.parse_expr(p))
                    .collect::<Result<Vec<_>, _>>()?; // Collect and propagate errors
                Ok(Expr::Call {
                    callee,
                    args,
                    ty: TypeId::default(),
                })
            }
            Rule::if_expr => {
                let mut inner = pair.into_inner();
                let cond = self.parse_expr(inner.next().unwrap())?;
                let then_branch = self.parse_expr(inner.next().unwrap())?;
                let else_branch = self.parse_expr(inner.next().unwrap())?;
                Ok(Expr::If {
                    cond: Box::new(cond),
                    then_branch: Box::new(then_branch),
                    else_branch: Box::new(else_branch),
                    ty: TypeId::default(),
                })
            }

            Rule::primary => {
                let text = pair.as_str();
                let mut inner = pair.into_inner();

                // Tokens like "null", "this", "Self", "true", "false" have no inner pairs
                if inner.peek().is_none() {
                    match text {
                        "null" => Ok(Expr::Literal(Literal::Null, TypeId::default())),
                        "true" | "false" => Self::parse_bool_literal(text),
                        // "this" => Ok(Expr::This(TypeId::default())),
                        // "Self" => Ok(Expr::SelfType),
                        other => Err(parse_error!("Unknown primary keyword: {}", other)),
                    }
                } else {
                    // Otherwise, unwrap and parse the inner rule
                    let inner_pair = inner.next().unwrap();
                    match inner_pair.as_rule() {
                        Rule::identifier => Ok(Expr::Identifier(
                            inner_pair.as_str().to_string(),
                            TypeId::default(),
                        )),
                        Rule::literal
                        | Rule::function_call_expr
                        | Rule::array_literal
                        | Rule::struct_init
                        | Rule::union_init
                        | Rule::tuple_literal
                        | Rule::if_expr
                        | Rule::range_expr
                        | Rule::string
                        | Rule::expr
                        | Rule::unary => self.parse_expr(inner_pair),
                        _ => Err(parse_error!(
                            "Unexpected primary inner rule: {:?}",
                            inner_pair.as_rule()
                        )),
                    }
                }
            }

            Rule::postfix => {
                // postfix = { primary ~ (postfix_field)* }
                let mut inner = pair.into_inner();
                let mut expr = self.parse_expr(inner.next().unwrap())?;

                // Handle chained field access (.field1.field2.field3)
                for field_pair in inner {
                    if field_pair.as_rule() == Rule::postfix_field {
                        let mut field_inner = field_pair.into_inner();
                        let field_name = field_inner
                            .next()
                            .ok_or(parse_error!("Expected field name after '.'"))?
                            .as_str()
                            .to_string();
                        expr = Expr::FieldAccess {
                            expr: Box::new(expr),
                            field_name,
                            ty: TypeId::default(),
                        };
                    }
                }

                Ok(expr)
            }

            Rule::struct_init => self.parse_struct_init(pair),

            Rule::range_expr => {
                let mut inner = pair.into_inner();
                let start = self.parse_expr(inner.next().unwrap())?;
                let end = self.parse_expr(inner.next().unwrap())?;
                Ok(Expr::RangeLiteral {
                    start: Box::new(start),
                    end: Box::new(end),
                })
            }
            other => Err(CompilationError::ParseError(format!(
                "Unexpected expr rule: {other:?}"
            ))),
        }
    }

    /// Parses an assignment expression (right-associative).
    fn parse_assign_expr(&self, pair: Pair<Rule>) -> CompileResult<Expr> {
        let mut inner = pair.into_inner();
        let left_pair = inner.next().unwrap(); // This is always the LHS (logical_or)

        let left = self.parse_expr(left_pair)?;

        // The grammar rule for `assign` is `assign = { logical_or ~ ASSIGN_OP ~ assign | logical_or }`.
        // This means `inner.next()` after the LHS (`logical_or`) will yield `ASSIGN_OP` if it's an assignment.
        // If there's no assignment, `inner` will be exhausted.

        if let Some(assign_op_pair) = inner.next() {
            // This should be ASSIGN_OP
            let op = AssignmentOperator::from_rule_pair(&assign_op_pair)?;
            let right_assign_expr_pair = inner.next().unwrap(); // This should be the RHS (nested assign)
            let right = self.parse_assign_expr(right_assign_expr_pair)?;
            Ok(Expr::Assign {
                op,
                left: Box::new(left),
                right: Box::new(right),
                ty: TypeId::default(),
            })
        } else {
            // No assignment operator, so it's just the expression itself (the logical_or that formed the LHS)
            Ok(left)
        }
    }

    fn parse_struct_init(&self, pair: Pair<Rule>) -> CompileResult<Expr> {
        // struct_init = { "struct" ~ type_annotation ~ "{" ~ (field_init ~ ("," ~ field_init)*)? ~ "}" }
        let mut inner = pair.into_inner();

        // Parse type annotation to get the struct type
        let type_pair = inner.next().unwrap();
        let struct_ty = self.parse_type(type_pair)?;

        // Parse field initializers
        let fields: Vec<FieldInit> = inner
            .filter(|p| p.as_rule() == Rule::field_init)
            .map(|p| self.parse_field_init(p))
            .collect::<Result<_, _>>()?;

        Ok(Expr::StructInit {
            ty: TypeId::default(),
            struct_type: Some(struct_ty),
            fields,
        })
    }

    fn parse_field_init(&self, pair: Pair<Rule>) -> CompileResult<FieldInit> {
        // field_init = { identifier ~ ":" ~ expr }
        let mut inner = pair.into_inner();
        let name = inner.next().unwrap().as_str().to_string();
        let value = self.parse_expr(inner.next().unwrap())?;
        Ok(FieldInit { name, value })
    }

    fn parse_bool_literal(s: &str) -> CompileResult<Expr> {
        match s {
            "true" => Ok(Expr::Literal(Literal::Bool(true), TypeId::default())),
            "false" => Ok(Expr::Literal(Literal::Bool(false), TypeId::default())),
            _ => Err(parse_error!("invalid boolean literal: {}", s)),
        }
    }

    fn unescape(s: impl AsRef<str>) -> Option<String> {
        // TODO: search if there is an escape sequence beforehand to
        // avoid the allocation and search logic
        let s = s.as_ref();
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next()? {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '\\' => out.push('\\'),
                    '\'' => out.push('\''),
                    '"' => out.push('"'),
                    // For this grammar, `\` escapes any character
                    other => out.push(other),
                }
            } else {
                out.push(c);
            }
        }
        Some(out)
    }
}
