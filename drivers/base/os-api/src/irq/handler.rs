// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device IRQ callback and outcome contracts, independent of host dispatch.

/// A small opaque identifier for a logical interrupt event source.
///
/// Drivers assign meaning to these (e.g. a NIC may use 0 = rx queue, 1 = tx
/// queue). The capability layer treats them as opaque indices into an
/// [`IrqEvent`] source bitmap and never interprets the value.
pub type IrqEventSource = u8;

/// Maximum number of distinct event sources a single [`IrqEvent`] can describe.
pub const IRQ_EVENT_SOURCES: usize = 8;

/// Outcome reported by an interrupt handler, augmented with which logical
/// event sources fired and whether a threaded handler should run.
///
/// Carries whether the handler claimed the interrupt, a small bitmap of which
/// logical sources triggered, and a wake-thread bit for threaded IRQ primary
/// handlers. The interpretation of each source bit is left to the driver. The
/// value is `Copy` and allocation-free, so it is safe to construct and return
/// from interrupt context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqEvent {
    handled: bool,
    sources: u8,
    wake_thread: bool,
}

impl IrqEvent {
    /// The handler claimed the interrupt but reports no specific source.
    pub const HANDLED: Self = Self {
        handled: true,
        sources: 0,
        wake_thread: false,
    };
    /// The handler did not claim the interrupt (shared-line fallback).
    pub const NOT_HANDLED: Self = Self {
        handled: false,
        sources: 0,
        wake_thread: false,
    };
    /// The handler claimed the interrupt and requests its threaded handler.
    pub const WAKE_THREAD: Self = Self {
        handled: true,
        sources: 0,
        wake_thread: true,
    };

    /// Claim the interrupt with the given source bitmask (bit `i` set ⇒ source
    /// `i` fired).
    pub const fn from_sources(sources: u8) -> Self {
        Self {
            handled: true,
            sources,
            wake_thread: false,
        }
    }

    /// Claim the interrupt with source bits and request the threaded handler.
    pub const fn wake_thread_from_sources(sources: u8) -> Self {
        Self {
            handled: true,
            sources,
            wake_thread: true,
        }
    }

    /// Whether this handler claimed and serviced the interrupt.
    pub const fn handled(&self) -> bool {
        self.handled
    }

    /// The raw source bitmask.
    pub const fn sources(&self) -> u8 {
        self.sources
    }

    /// Whether this handler requests its threaded IRQ handler.
    pub const fn wake_thread(&self) -> bool {
        self.wake_thread
    }

    /// Whether source `src` is reported as fired.
    pub fn has_source(&self, src: IrqEventSource) -> bool {
        src < IRQ_EVENT_SOURCES as u8 && (self.sources & (1 << src)) != 0
    }

    /// Combine another event into this one (OR the source bits; handled if
    /// either is). Useful for shared-line dispatch aggregating handlers.
    pub fn merge(&mut self, other: IrqEvent) {
        self.handled |= other.handled;
        self.sources |= other.sources;
        self.wake_thread |= other.wake_thread;
    }
}

impl Default for IrqEvent {
    fn default() -> Self {
        Self::HANDLED
    }
}

/// A device interrupt handler.
///
/// Handlers run in interrupt context: they must not block, must not allocate,
/// and should defer heavy work to a thread. The argument is the OS-visible IRQ
/// number that triggered the handler. Any closure that is
/// `Fn(usize) -> IrqEvent + Send + Sync` implements this trait.
pub trait IrqHandler: Send + Sync {
    /// Service a fired interrupt and report which event sources fired.
    fn handle(&self, irq: usize) -> IrqEvent;
}

impl<F> IrqHandler for F
where
    F: Fn(usize) -> IrqEvent + Send + Sync,
{
    fn handle(&self, irq: usize) -> IrqEvent {
        self(irq)
    }
}

/// A sleepable device threaded IRQ handler.
///
/// Thread handlers run in task context after the primary IRQ handler returns
/// [`IrqEvent::WAKE_THREAD`] or [`IrqEvent::wake_thread_from_sources`]. They may
/// use sleepable primitives, but must still obey the owning driver's teardown
/// protocol because a supporting provider must drain the IRQ thread before freeing
/// the action. This trait does not imply host support for threaded registration.
pub trait IrqThreadHandler: Send + Sync {
    /// Service deferred interrupt work in task context.
    fn handle(&self, irq: usize) -> IrqEvent;
}

impl<F> IrqThreadHandler for F
where
    F: Fn(usize) -> IrqEvent + Send + Sync,
{
    fn handle(&self, irq: usize) -> IrqEvent {
        self(irq)
    }
}
