// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Where a value in a MIR body comes from, following locals that are
//! assigned once: links function values to the calls, parameters and fields
//! they flow through, which narrows fn-pointer and generic `Fn*` calls.

use std::collections::HashMap;

use rustc_abi::FIRST_VARIANT;
use rustc_hir::def_id::DefId;
use rustc_middle::{
    mir::{self, AggregateKind, Local, Operand, PlaceRef, PlaceTy, ProjectionElem, Rvalue},
    ty::{self, TyCtxt},
};

/// Longest chain of single assignments followed.
const MAX_DEPTH: usize = 8;

/// The source of a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    /// The body's parameter at this index (0-based).
    Param(usize),
    /// Read from this field.
    Field(DefId),
    /// This function item.
    Fn(DefId),
    /// This closure.
    Closure(DefId),
    Unknown,
}

/// Single-assignment view of one body.
pub(super) struct Flow<'b, 'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'b mir::Body<'tcx>,
    /// The rvalue of locals assigned exactly once by a statement; `None` for
    /// locals assigned more than once or written by a call.
    assigned: HashMap<Local, Option<&'b Rvalue<'tcx>>>,
}

impl<'b, 'tcx> Flow<'b, 'tcx> {
    pub(super) fn new(tcx: TyCtxt<'tcx>, body: &'b mir::Body<'tcx>) -> Self {
        let mut assigned: HashMap<Local, Option<&'b Rvalue<'tcx>>> = HashMap::new();
        for block in body.basic_blocks.iter() {
            for stmt in &block.statements {
                let mir::StatementKind::Assign(assign) = &stmt.kind else {
                    continue;
                };
                let (place, rvalue) = &**assign;
                if let Some(local) = place.as_local() {
                    assigned
                        .entry(local)
                        .and_modify(|slot| *slot = None)
                        .or_insert(Some(rvalue));
                }
            }
            if let Some(mir::Terminator {
                kind: mir::TerminatorKind::Call { destination, .. },
                ..
            }) = &block.terminator
            {
                assigned.insert(destination.local, None);
            }
        }
        Self {
            tcx,
            body,
            assigned,
        }
    }

    pub(super) fn operand(&self, operand: &Operand<'tcx>) -> Origin {
        self.operand_at(operand, 0)
    }

    fn operand_at(&self, operand: &Operand<'tcx>, depth: usize) -> Origin {
        match operand {
            Operand::Constant(constant) => match constant.const_.ty().kind() {
                ty::FnDef(def, _) => Origin::Fn(*def),
                ty::Closure(def, _) => Origin::Closure(*def),
                _ => Origin::Unknown,
            },
            Operand::Copy(place) | Operand::Move(place) => self.place_at(place.as_ref(), depth),
            Operand::RuntimeChecks(_) => Origin::Unknown,
        }
    }

    fn place_at(&self, place: PlaceRef<'tcx>, depth: usize) -> Origin {
        if depth > MAX_DEPTH {
            return Origin::Unknown;
        }
        if let Some(field) = self.field_of(place) {
            return Origin::Field(field);
        }
        if place
            .projection
            .iter()
            .any(|elem| !matches!(elem, ProjectionElem::Deref))
        {
            return Origin::Unknown;
        }
        let index = place.local.as_usize();
        if (1..=self.body.arg_count).contains(&index) {
            return Origin::Param(index - 1);
        }
        let Some(Some(rvalue)) = self.assigned.get(&place.local) else {
            return Origin::Unknown;
        };
        match rvalue {
            Rvalue::Use(operand) | Rvalue::Cast(_, operand, _) => {
                self.operand_at(operand, depth + 1)
            }
            Rvalue::Ref(_, _, inner) | Rvalue::RawPtr(_, inner) => {
                self.place_at(inner.as_ref(), depth + 1)
            }
            Rvalue::Aggregate(kind, _) => match **kind {
                AggregateKind::Closure(def, _) => Origin::Closure(def),
                _ => Origin::Unknown,
            },
            _ => Origin::Unknown,
        }
    }

    /// The ADT field that `place` ends in, if its last field projection is
    /// on a struct, union or enum variant.
    pub(super) fn field_of(&self, place: PlaceRef<'tcx>) -> Option<DefId> {
        let mut place_ty = PlaceTy::from_ty(self.body.local_decls[place.local].ty);
        let mut field = None;
        for elem in place.projection {
            if let ProjectionElem::Field(index, _) = elem {
                field = match place_ty.ty.kind() {
                    ty::Adt(adt, _) => {
                        let variant = place_ty.variant_index.unwrap_or(FIRST_VARIANT);
                        Some(adt.variant(variant).fields[*index].did)
                    }
                    _ => None,
                };
            }
            place_ty = place_ty.projection_ty(self.tcx, *elem);
        }
        field
    }
}
