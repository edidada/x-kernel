// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal IRQ action model.

use bitflags::bitflags;

use super::thread::IrqThreadSlot;
use crate::{IrqEvent, Virq, platform::Handler};

/// Public identity for one registered IRQ action on a line.
///
/// The token is interpreted together with the IRQ line it was returned for.
/// It is the `kirq` equivalent of Linux's `dev_id` removal key for shared IRQs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqActionToken(usize);

impl IrqActionToken {
    /// Creates an action token from a line-local numeric id.
    pub const fn new(id: usize) -> Self {
        Self(id)
    }

    /// Returns the line-local numeric id.
    pub const fn id(self) -> usize {
        self.0
    }
}

/// Internal identity for a registered IRQ action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IrqActionId(IrqActionToken);

impl IrqActionId {
    const REGULAR: Self = Self(IrqActionToken::new(0));

    const fn shared(token: IrqActionToken) -> Self {
        Self(token)
    }

    const fn token(self) -> IrqActionToken {
        self.0
    }
}

bitflags! {
    /// Flags owned by an IRQ action, distinct from resource-level `IrqFlags`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct IrqActionFlags: u32 {
        const SHARED = 1 << 0;
        const ONESHOT = 1 << 1;
        const PER_CPU = 1 << 2;
        const NO_THREAD = 1 << 3;
    }
}

/// One internally registered IRQ action.
#[derive(Clone)]
pub(super) struct IrqAction {
    #[allow(dead_code)]
    id: IrqActionId,
    primary: Handler,
    thread: Option<IrqThreadSlot>,
    #[allow(dead_code)]
    flags: IrqActionFlags,
    #[allow(dead_code)]
    name: Option<&'static str>,
}

impl IrqAction {
    /// Builds the current public single-handler registration as one action.
    pub(super) fn regular(primary: Handler) -> Self {
        Self {
            id: IrqActionId::REGULAR,
            primary,
            thread: None,
            flags: IrqActionFlags::empty(),
            name: None,
        }
    }

    /// Builds one shared IRQ action with an explicit removal token.
    pub(super) fn shared(token: IrqActionToken, primary: Handler) -> Self {
        Self {
            id: IrqActionId::shared(token),
            primary,
            thread: None,
            flags: IrqActionFlags::SHARED,
            name: None,
        }
    }

    /// Builds one regular IRQ action with a dedicated threaded handler.
    pub(super) fn threaded(
        primary: Handler,
        thread: IrqThreadSlot,
        name: Option<&'static str>,
    ) -> Self {
        Self {
            id: IrqActionId::REGULAR,
            primary,
            thread: Some(thread),
            flags: IrqActionFlags::ONESHOT,
            name,
        }
    }

    /// Returns this action's line-local removal token.
    pub(super) fn token(&self) -> IrqActionToken {
        self.id.token()
    }

    /// Returns whether this action participates in shared IRQ fanout.
    pub(super) fn is_shared(&self) -> bool {
        self.flags.contains(IrqActionFlags::SHARED)
    }

    /// Returns the primary handler.
    pub(super) fn primary(&self) -> &Handler {
        &self.primary
    }

    /// Takes ownership of the primary handler.
    pub(super) fn into_primary(self) -> Handler {
        self.primary
    }

    /// Returns the threaded handler slot, if this action has one.
    pub(super) fn thread_slot(&self) -> Option<&IrqThreadSlot> {
        self.thread.as_ref()
    }

    /// Runs the primary handler and classifies its return value.
    pub(super) fn run_primary(&self, irq: Virq) -> IrqActionReturn {
        IrqActionReturn::from_event(self.primary().handle(irq), self.thread.is_some())
    }

    /// Stops the threaded handler, if present.
    pub(super) fn stop_thread(&self) {
        if let Some(thread) = &self.thread {
            thread.stop();
        }
    }

    /// Waits for the threaded handler task to exit, if present.
    pub(super) fn wait_thread_stopped(
        &self,
        operation: &'static str,
    ) -> Result<(), kpoll::PollRegisterError> {
        if let Some(thread) = &self.thread {
            thread.wait_stopped(operation)?;
        }
        Ok(())
    }

    #[cfg(unittest)]
    pub(super) const fn id(&self) -> IrqActionId {
        self.id
    }

    #[cfg(unittest)]
    pub(super) const fn flags(&self) -> IrqActionFlags {
        self.flags
    }

    #[cfg(unittest)]
    pub(super) const fn name(&self) -> Option<&'static str> {
        self.name
    }
}

/// Internal classification of a primary IRQ handler result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IrqActionReturn {
    /// The primary handler did not claim the interrupt.
    Unhandled,
    /// The primary handler claimed the interrupt.
    Handled { sources: u8 },
    /// The primary handler claimed the interrupt and requested thread wakeup.
    WakeThread { sources: u8 },
}

impl IrqActionReturn {
    /// Classifies the current public handler return value.
    pub(super) const fn from_event(event: IrqEvent, has_thread: bool) -> Self {
        if event.handled() {
            if has_thread {
                Self::WakeThread {
                    sources: event.sources(),
                }
            } else {
                Self::Handled {
                    sources: event.sources(),
                }
            }
        } else {
            Self::Unhandled
        }
    }

    /// Returns the source bitmap carried by handled-like outcomes.
    #[allow(dead_code)]
    pub(super) const fn sources(self) -> u8 {
        match self {
            Self::Unhandled => 0,
            Self::Handled { sources } | Self::WakeThread { sources } => sources,
        }
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use alloc::sync::Arc;

    use unittest::def_test;

    use super::{IrqAction, IrqActionFlags, IrqActionId, IrqActionReturn, IrqActionToken};
    use crate::{IrqEvent, Virq, runtime::thread::IrqThreadSlot};

    fn handled_handler(_irq: Virq) -> IrqEvent {
        IrqEvent::HANDLED
    }

    #[def_test]
    fn test_irq_action_regular_construction_is_non_threaded() {
        let action = IrqAction::regular(Arc::new(handled_handler));

        assert_eq!(action.id(), IrqActionId::REGULAR);
        assert_eq!(action.token(), IrqActionToken::new(0));
        assert_eq!(action.flags(), IrqActionFlags::empty());
        assert_eq!(action.name(), None);
        assert!(action.thread_slot().is_none());
    }

    #[def_test]
    fn test_irq_action_shared_construction_has_token() {
        let token = IrqActionToken::new(7);
        let action = IrqAction::shared(token, Arc::new(handled_handler));

        assert_eq!(action.token(), token);
        assert_eq!(action.flags(), IrqActionFlags::SHARED);
        assert!(action.is_shared());
        assert!(action.thread_slot().is_none());
    }

    #[def_test]
    fn test_irq_action_threaded_construction_has_thread_slot() {
        let thread = IrqThreadSlot::test_new(0x55, Arc::new(handled_handler));
        let action = IrqAction::threaded(Arc::new(handled_handler), thread, Some("future"));

        assert_eq!(action.id(), IrqActionId::REGULAR);
        assert_eq!(action.flags(), IrqActionFlags::ONESHOT);
        assert_eq!(action.name(), Some("future"));
        assert!(action.thread_slot().is_some());
    }

    #[def_test]
    fn test_irq_action_return_maps_public_events() {
        assert_eq!(
            IrqActionReturn::from_event(IrqEvent::HANDLED, false),
            IrqActionReturn::Handled { sources: 0 }
        );
        assert_eq!(
            IrqActionReturn::from_event(IrqEvent::from_sources(0b1010_0001), false),
            IrqActionReturn::Handled {
                sources: 0b1010_0001
            }
        );
        assert_eq!(
            IrqActionReturn::from_event(IrqEvent::NOT_HANDLED, false),
            IrqActionReturn::Unhandled
        );
    }

    #[def_test]
    fn test_not_handled_is_not_wake_thread() {
        let action_return = IrqActionReturn::from_event(IrqEvent::NOT_HANDLED, true);

        assert!(!matches!(action_return, IrqActionReturn::WakeThread { .. }));
    }

    #[def_test]
    fn test_wake_thread_return_keeps_sources() {
        let action_return = IrqActionReturn::WakeThread { sources: 0x5a };

        assert_eq!(action_return.sources(), 0x5a);
    }
}
