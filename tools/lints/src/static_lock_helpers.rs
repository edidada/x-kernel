// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared type, macro-origin and initializer helpers for static lock lints.

use rustc_hir::{Expr, ExprKind};
use rustc_middle::ty::{self, Ty, TyCtxt, TypeckResults};
use rustc_span::{ExpnId, Span};

/// Returns whether `ty` resolves to the named type from the named crate.
pub(crate) fn is_type(tcx: TyCtxt<'_>, ty: Ty<'_>, crate_name: &str, type_name: &str) -> bool {
    let ty::Adt(definition, _) = ty.kind() else {
        return false;
    };
    let definition = definition.did();
    tcx.crate_name(definition.krate).as_str() == crate_name
        && tcx.item_name(definition).as_str() == type_name
}

/// Returns whether `definition` was produced by the real `static_lock!`.
pub(crate) fn defined_by_static_lock(
    tcx: TyCtxt<'_>,
    definition: rustc_hir::def_id::DefId,
) -> bool {
    // Passthrough `$item` macros retain source spans; resolver provenance still
    // records the expansion that defined the item, including macro aliases.
    let mut expansion = tcx.expn_that_defined(definition);
    while expansion != ExpnId::root() {
        let data = expansion.expn_data();
        if data.macro_def_id.is_some_and(|macro_definition| {
            matches!(
                tcx.crate_name(macro_definition.krate).as_str(),
                "ksync" | "kspin" | "klockstat_macros"
            ) && tcx.item_name(macro_definition).as_str() == "static_lock"
        }) {
            return true;
        }
        expansion = data.parent;
    }
    false
}

/// Strips statement-free `{ .. }` and `unsafe { .. }` blocks.
pub(crate) fn peel_blocks<'tcx>(mut expression: &'tcx Expr<'tcx>) -> &'tcx Expr<'tcx> {
    while let ExprKind::Block(block, _) = expression.kind
        && block.stmts.is_empty()
        && let Some(inner) = block.expr
    {
        expression = inner;
    }
    expression
}

/// Returns the callee span when `expression` calls `method_name` on `type_name`.
pub(crate) fn direct_call_to_type<'tcx>(
    tcx: TyCtxt<'tcx>,
    types: &TypeckResults<'tcx>,
    expression: &Expr<'tcx>,
    crate_name: &str,
    type_name: &str,
    method_name: &str,
) -> Option<Span> {
    let ExprKind::Call(callee, _) = expression.kind else {
        return None;
    };
    let ty::FnDef(function, _) = *types.expr_ty(callee).kind() else {
        return None;
    };
    if tcx.item_name(function).as_str() != method_name {
        return None;
    }
    let implementation = tcx.impl_of_assoc(function)?;
    is_type(
        tcx,
        tcx.type_of(implementation).instantiate_identity(),
        crate_name,
        type_name,
    )
    .then_some(callee.span)
}
