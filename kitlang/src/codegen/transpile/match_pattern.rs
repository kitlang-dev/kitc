use super::CodegenCtx;
use crate::codegen::ast::{self, Expr, ExprKind, Literal, MatchArm, MatchStmt};
use crate::codegen::name_mangling::mangle_enum_variant;
use crate::codegen::types::{Type, TypeId};

/// A variable binding (name, c_type, value).
pub type VariableBinding = (String, String, String);

/// Result of decomposing a pattern: a C condition and variable bindings.
struct PatternMatch {
    condition: String,
    bindings: Vec<VariableBinding>,
}

/// A match arm together with its pre-computed C condition and variable bindings.
struct ArmBindings<'a> {
    arm: &'a MatchArm,
    condition: String,
    bindings: Vec<VariableBinding>,
}

impl CodegenCtx<'_> {
    /// Transpile a match statement into C if/else-if/else chain.
    pub(super) fn transpile_match_stmt(&self, m: &MatchStmt) -> String {
        debug_assert!(!m.arms.is_empty(), "match statement with no arms");

        let matched = self.transpile_expr(&m.expr);
        let matched_ty = Self::expr_type_id(&m.expr);

        let mut arms: Vec<ArmBindings<'_>> = Vec::new();
        for arm in &m.arms {
            let PatternMatch {
                condition,
                bindings,
            } = self.decompose_pattern(&arm.pattern, matched_ty, &matched);
            arms.push(ArmBindings {
                arm,
                condition,
                bindings,
            });
        }

        let mut code = String::new();
        let mut first = true;
        for ArmBindings {
            arm,
            condition,
            bindings,
        } in &arms
        {
            let is_wildcard = matches!(
                &arm.pattern,
                Expr {
                    kind: ExprKind::Identifier { name, .. },
                    ..
                } if name == "default" || name == "_"
            );
            if is_wildcard {
                if !first {
                    code.push_str(" else ");
                }
                code.push_str(&self.transpile_match_body(&arm.body, bindings));
                first = false;
            } else if first {
                code.push_str(&format!("if ({condition}) "));
                code.push_str(&self.transpile_match_body(&arm.body, bindings));
                first = false;
            } else {
                code.push_str(&format!(" else if ({condition}) "));
                code.push_str(&self.transpile_match_body(&arm.body, bindings));
            }
        }
        code.push('\n');
        code
    }

    fn transpile_match_body(&self, body: &ast::Block, bindings: &[VariableBinding]) -> String {
        let mut code = String::from("{\n");
        for (name, ctype, value) in bindings {
            code.push_str(&format!("    {ctype} {name} = {value};\n"));
        }
        for stmt in &body.stmts {
            let stmt_code = self.transpile_stmt(stmt);
            for line in stmt_code.lines() {
                code.push_str("    ");
                code.push_str(line);
                code.push('\n');
            }
        }
        code.push('}');
        code
    }

    fn decompose_pattern(
        &self,
        pattern: &Expr,
        matched_ty: TypeId,
        matched_value: &str,
    ) -> PatternMatch {
        match &pattern.kind {
            ExprKind::Identifier { name } if name == "_" || name == "default" => PatternMatch {
                condition: "1".to_string(),
                bindings: vec![],
            },
            ExprKind::Identifier { name } => {
                self.decompose_identifier_pattern(name, matched_ty, matched_value)
            }
            ExprKind::Call { callee, args } => {
                if let ExprKind::Identifier { name: variant_name } = &callee.kind
                    && let Some(pm) = self.decompose_enum_variant_call_pattern(
                        variant_name,
                        args,
                        matched_ty,
                        matched_value,
                    )
                {
                    return pm;
                }
                let pattern_val = self.transpile_expr(pattern);
                PatternMatch {
                    condition: format!("{matched_value} == {pattern_val}"),
                    bindings: vec![],
                }
            }
            ExprKind::Literal { value } => {
                let lit_str = value.to_c();
                let condition = match value {
                    Literal::String(_s) => {
                        format!("strcmp({matched_value}, {lit_str}) == 0")
                    }
                    _ => format!("{matched_value} == {lit_str}"),
                };
                PatternMatch {
                    condition,
                    bindings: vec![],
                }
            }
            ExprKind::FieldAccess {
                expr, field_name, ..
            } => {
                let inner_value = format!("{matched_value}.{field_name}");
                let inner_ty = Self::expr_type_id(expr);
                self.decompose_pattern(expr, inner_ty, &inner_value)
            }
            _ => {
                let pattern_val = self.transpile_expr(pattern);
                PatternMatch {
                    condition: format!("{matched_value} == {pattern_val}"),
                    bindings: vec![],
                }
            }
        }
    }

    /// If `name` is an enum variant, return a PatternMatch comparing it against
    /// `matched_value`.  Returns `None` for non-variant identifiers (plain bindings).
    fn decompose_identifier_pattern(
        &self,
        name: &str,
        matched_ty: TypeId,
        matched_value: &str,
    ) -> PatternMatch {
        let info = match self
            .inferencer
            .symbols()
            .lookup_enum_variant_by_simple_name(name)
        {
            Some(info) => info,
            None => return self.binding_pattern(name, matched_ty, matched_value),
        };
        // The matched value's own type selects the monomorph (variant lookup
        // order for template/monomorph infos is not deterministic).
        let enum_name = match self.inferencer.store.resolve(matched_ty) {
            Ok(Type::Named(n)) if self.inferencer.is_monomorph_name(&n) => n,
            Ok(_) if self.inferencer.is_monomorph_name(&info.enum_name) => info.enum_name.clone(),
            _ => self.resolved_enum_name(matched_ty, &info.enum_name),
        };
        let enum_def = match self.inferencer.symbols().lookup_enum(&enum_name) {
            Some(def) => def,
            None => return self.binding_pattern(name, matched_ty, matched_value),
        };
        let all_simple = enum_def.variants.iter().all(|v| v.args.is_empty());
        let mangled = mangle_enum_variant(&self.current_module, &enum_name, name);

        if all_simple {
            return PatternMatch {
                condition: format!("{matched_value} == {mangled}"),
                bindings: vec![],
            };
        }
        if info.arg_types.is_empty() {
            let discriminant = format!("{matched_value}._discriminant");
            return PatternMatch {
                condition: format!("{discriminant} == {mangled}"),
                bindings: vec![],
            };
        }
        // Variant has args but we're just a bare identifier – treat as binding.
        self.binding_pattern(name, matched_ty, matched_value)
    }

    /// Produce a binding pattern: matches everything and binds `name` to `matched_value`.
    fn binding_pattern(&self, name: &str, matched_ty: TypeId, matched_value: &str) -> PatternMatch {
        let ctype = self.resolve_type_to_c_name(matched_ty, "int");
        PatternMatch {
            condition: "1".to_string(),
            bindings: vec![(name.to_string(), ctype, matched_value.to_string())],
        }
    }

    /// If `callee` is an enum variant constructor call, decompose it into a
    /// discriminant check and field bindings.  Returns `None` for non-variant calls.
    fn decompose_enum_variant_call_pattern(
        &self,
        variant_name: &str,
        args: &[Expr],
        matched_ty: TypeId,
        matched_value: &str,
    ) -> Option<PatternMatch> {
        let info = self
            .inferencer
            .symbols()
            .lookup_enum_variant_by_simple_name(variant_name)?;
        // The matched value's own type selects the monomorph (see
        // `decompose_identifier_pattern`).
        let enum_name = match self.inferencer.store.resolve(matched_ty) {
            Ok(Type::Named(n)) if self.inferencer.is_monomorph_name(&n) => n,
            Ok(_) if self.inferencer.is_monomorph_name(&info.enum_name) => info.enum_name.clone(),
            _ => self.resolved_enum_name(matched_ty, &info.enum_name),
        };
        let enum_def = self.inferencer.symbols().lookup_enum(&enum_name)?.clone();

        let all_simple = enum_def.variants.iter().all(|v| v.args.is_empty());
        let mangled = mangle_enum_variant(&self.current_module, &enum_name, variant_name);

        if all_simple {
            return Some(PatternMatch {
                condition: format!("{matched_value} == {mangled}"),
                bindings: vec![],
            });
        }

        // Complex enum: check discriminant and extract fields.
        let discriminant = format!("{matched_value}._discriminant");
        let variant_union = variant_name.to_lowercase();
        let variant_data = format!("{matched_value}._variant.{variant_union}");

        let variant_def = enum_def.variants.iter().find(|v| v.name == *variant_name);

        let mut conditions = vec![format!("{discriminant} == {mangled}")];
        let mut bindings = Vec::new();

        for (i, arg_pattern) in args.iter().enumerate() {
            debug_assert!(
                i < info.arg_types.len(),
                "pattern arg {} exceeds {} declared fields for variant {}",
                i,
                info.arg_types.len(),
                variant_name,
            );
            // Field types come from the (resolved, possibly monomorphized) enum
            // definition; the variant info registered for generic enums keeps
            // the raw `T`-style annotations.
            let field_ty = variant_def
                .and_then(|v| v.args.get(i))
                .map(|a| a.ty)
                .unwrap_or(info.arg_types[i]);
            let field_name = variant_def
                .and_then(|vd| vd.args.get(i))
                .map(|a| &a.name)
                .cloned()
                .unwrap_or_else(|| format!("arg{i}"));
            let field_value = format!("{variant_data}.{field_name}");
            let inner = self.decompose_pattern(arg_pattern, field_ty, &field_value);
            if !inner.condition.is_empty() && inner.condition != "1" {
                conditions.push(inner.condition);
            }
            bindings.extend(inner.bindings);
        }

        let condition = if conditions.is_empty() {
            "1".to_string()
        } else {
            conditions.join(" && ")
        };

        Some(PatternMatch {
            condition,
            bindings,
        })
    }
}
