//! Specialization via `default Trait as Type` declarations.
//!
//! A `default Trait as Type` declaration (`DefaultSpecialization`) lets an otherwise-unresolvable
//! type variable that is constrained by `Trait` be specialized to `Type` so the monomorphization
//! fixpoint keeps making progress. In Kit's value model a type variable with no supplied type
//! argument and no other source of information can only be resolved by such a default, so the
//! resolution is applied eagerly when the missing argument is created (see
//! `instance_params_and_type` in `monomorph.rs`). The helper here answers "given a set of trait
//! constraints, which default applies?".

use crate::codegen::ast::DefaultSpecialization;
use crate::codegen::monomorph::constraint_trait_name;
use crate::codegen::types::Type;

/// Given a list of trait constraints on a type variable, return the default type for the first
/// constraint whose trait has a registered `default Trait as Type` specialization. Returns `None`
/// when no constraint is defaultable, so callers fall back to ordinary inference.
pub(crate) fn default_for_constraints(
    constraints: &[Type],
    defaults: &[DefaultSpecialization],
) -> Option<Type> {
    for constraint in constraints {
        let Some(trait_name) = constraint_trait_name(constraint) else {
            continue;
        };
        if let Some(default) = defaults.iter().find(|d| d.trait_name == trait_name) {
            return Some(default.default_type.clone());
        }
    }
    None
}
