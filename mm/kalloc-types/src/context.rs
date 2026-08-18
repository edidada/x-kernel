// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Allocation-context contract carried by fallible heap APIs.
///
/// Only [`AllocContext::Sleepable`] is supported for **new allocations** in
/// the first implementation milestone. Operations that can prove they do not
/// allocate, such as pushing into an already-reserved buffer, are valid in
/// every context. The remaining variants reserve API shape for later phases
/// so call sites do not need a semantic rewrite when no-wait or
/// reserve-backed allocation policies are added.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AllocContext {
    /// The caller is allowed to run an allocation path that may sleep.
    #[default]
    Sleepable,
    /// The caller must not rely on sleeping allocation behavior.
    NoWait,
    /// The caller is in a context that will later need atomic-safe behavior.
    Atomic,
    /// The caller expects a future reserve-backed critical allocation mode.
    ReservedCritical,
}

pub(crate) fn require_supported_context(context: AllocContext) -> Result<(), crate::AllocError> {
    match context {
        AllocContext::Sleepable => Ok(()),
        AllocContext::NoWait | AllocContext::Atomic | AllocContext::ReservedCritical => {
            Err(crate::AllocError::UnsupportedContext)
        }
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::{AllocContext, require_supported_context};
    use crate::AllocError;

    #[def_test]
    fn default_context_is_sleepable() {
        assert_eq!(AllocContext::default(), AllocContext::Sleepable);
    }

    #[def_test]
    fn rejects_phase2_contexts_for_now() {
        assert_eq!(
            require_supported_context(AllocContext::NoWait),
            Err(AllocError::UnsupportedContext)
        );
        assert_eq!(
            require_supported_context(AllocContext::Atomic),
            Err(AllocError::UnsupportedContext)
        );
        assert_eq!(
            require_supported_context(AllocContext::ReservedCritical),
            Err(AllocError::UnsupportedContext)
        );
    }
}
