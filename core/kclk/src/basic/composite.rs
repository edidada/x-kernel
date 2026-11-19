// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Composite clock: combines independent mux, divider, and gate sub-nodes.

use alloc::sync::Arc;

use crate::{ClkHwOps, ClkRateRequest, ClkResult, Rate, RoundResult};

/// A clock composed of up to three independent sub-nodes: a [`Mux`](super::Mux)
/// that selects the parent, a [`Divider`](super::Divider) that sets the rate,
/// and a [`Gate`](super::Gate) that enables/disables.
///
/// Each sub-role is optional; a role that is absent falls back to the
/// [`ClkHwOps`] default (pass-through / not-supported) for the operations it
/// would have served. This lets a controller describe a node that has, say, a
/// divider and a gate but a fixed parent, with a single registration.
pub struct Composite {
    mux: Option<Arc<dyn ClkHwOps>>,
    div: Option<Arc<dyn ClkHwOps>>,
    gate: Option<Arc<dyn ClkHwOps>>,
}

impl Composite {
    /// Create a composite clock from optional sub-node roles.
    pub fn new(
        mux: Option<Arc<dyn ClkHwOps>>,
        div: Option<Arc<dyn ClkHwOps>>,
        gate: Option<Arc<dyn ClkHwOps>>,
    ) -> Self {
        Self { mux, div, gate }
    }

    /// Wrap the composite as the provider-side ops object.
    pub fn into_ops(self) -> Arc<dyn ClkHwOps> {
        Arc::new(self)
    }
}

impl ClkHwOps for Composite {
    // ---- gating: delegate to the gate role ----
    fn prepare(&self) -> ClkResult {
        match &self.gate {
            Some(g) => g.prepare(),
            None => Ok(()),
        }
    }

    fn unprepare(&self) -> ClkResult {
        match &self.gate {
            Some(g) => g.unprepare(),
            None => Ok(()),
        }
    }

    fn enable(&self) -> ClkResult {
        match &self.gate {
            Some(g) => g.enable(),
            None => Ok(()),
        }
    }

    fn disable(&self) {
        if let Some(g) = &self.gate {
            g.disable();
        }
    }

    fn is_enabled(&self) -> ClkResult<bool> {
        match &self.gate {
            Some(g) => g.is_enabled(),
            None => Ok(false),
        }
    }

    // ---- rate: delegate to the divider role ----
    fn recalc_rate(&self, parent_rate: Rate) -> ClkResult<Rate> {
        match &self.div {
            Some(d) => d.recalc_rate(parent_rate),
            None => Ok(parent_rate),
        }
    }

    fn round_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<RoundResult> {
        match &self.div {
            Some(d) => d.round_rate(rate, parent_rate),
            // No divider: transparent, ask the parent for `rate`.
            None => Ok(RoundResult {
                rate: parent_rate,
                parent_target: Some(rate),
            }),
        }
    }

    fn determine_rate(&self, req: &mut ClkRateRequest, parents: &[(u8, Rate)]) -> ClkResult<()> {
        // Let the mux pick the best parent (if any), then let the divider
        // compute the achievable rate on that chosen parent. Without this the
        // default determine_rate delegates to round_rate against the *current*
        // parent, so a mux+divider composite would never switch sources.
        //
        // Capture the requested rate first: the mux is transparent and sets
        // req.rate to the chosen parent's rate, so the divider must be queried
        // with the *original* request, not the clobbered req.rate.
        let requested = req.rate;
        let mut have_div = false;
        if let Some(mux) = &self.mux {
            mux.determine_rate(req, parents)?;
        }
        if let Some(div) = &self.div {
            let rr = div.round_rate(requested, req.best_parent_rate)?;
            req.rate = rr.rate;
            // The divider satisfies the request on the chosen parent; it does
            // not ask the framework to change that parent's rate.
            req.parent_target = rr.parent_target;
            have_div = true;
        }
        if !have_div {
            // Gate-only (or bare) composite: the node is rate-transparent, so
            // mirror `round_rate` — keep the current parent rate as the
            // achievable rate and ask the parent to provide the request, in
            // step with the SET_RATE_PARENT propagation. Leaving the raw
            // request here would report an unachievable rate.
            req.rate = req.best_parent_rate;
            req.parent_target = Some(requested);
        }
        Ok(())
    }

    fn set_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<Rate> {
        match &self.div {
            Some(d) => d.set_rate(rate, parent_rate),
            None => Err(crate::ClkError::NotSupported),
        }
    }

    // ---- parent selection: delegate to the mux role ----
    fn num_parents(&self) -> u8 {
        match &self.mux {
            Some(m) => m.num_parents(),
            None => 1,
        }
    }

    fn get_parent(&self) -> ClkResult<u8> {
        match &self.mux {
            Some(m) => m.get_parent(),
            None => Ok(0),
        }
    }

    fn set_parent(&self, index: u8) -> ClkResult {
        match &self.mux {
            Some(m) => m.set_parent(index),
            None => Err(crate::ClkError::NotSupported),
        }
    }
}
