// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Hardware event sources.

use alloc::vec::Vec;

use kdevice::{
    SystemDeviceDescriptor, SystemDeviceId, install_system_device, remove_system_device,
};
use kerrno::{KErrorKind, KResult};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use crate::controller_for;

/// Receives hardware hotplug notifications.
///
/// A source may call these methods from process or interrupt context. The sink
/// performs no allocation beyond what `kdevice` itself does and takes only the
/// registry locks, so interrupt-context callers must be prepared for those
/// short critical sections; the preferred contract is to defer to a work item.
pub trait HotplugSink: Send + Sync {
    /// A device became present.
    fn on_added(&self, descriptor: SystemDeviceDescriptor) -> KResult<()>;

    /// A device is no longer present. Its sysfs directory must disappear.
    fn on_removed(&self, id: SystemDeviceId) -> KResult<()>;

    /// Firmware or a hypervisor asks the kernel to change online state.
    ///
    /// The sink forwards this to the class controller, which revalidates it.
    fn on_online_requested(&self, id: SystemDeviceId, is_online: bool) -> KResult<()>;
}

/// A hardware source of CPU/memory hotplug events.
///
/// `start` must not fabricate events. A source that cannot observe real
/// hardware belongs nowhere in this tree.
pub trait HotplugEventSource: Send + Sync {
    /// Installs `sink` and starts delivering events.
    ///
    /// Called at most once per source by [`start_event_sources`].
    fn start(&self, sink: &'static dyn HotplugSink) -> KResult<()>;
}

struct SystemHotplugSink;

static SINK: SystemHotplugSink = SystemHotplugSink;

impl HotplugSink for SystemHotplugSink {
    fn on_added(&self, descriptor: SystemDeviceDescriptor) -> KResult<()> {
        // Reinstallation is idempotent: a firmware source may repeat an
        // `Added` notification for a device that is already present.
        let _ = install_system_device(descriptor);
        Ok(())
    }

    fn on_removed(&self, id: SystemDeviceId) -> KResult<()> {
        remove_system_device(id)
    }

    fn on_online_requested(&self, id: SystemDeviceId, is_online: bool) -> KResult<()> {
        controller_for(id.class())
            .ok_or(KErrorKind::OperationNotSupported)?
            .set_online(id, is_online)
    }
}

struct EventSourceEntry {
    source: &'static dyn HotplugEventSource,
    is_started: bool,
}

static EVENT_SOURCES: LazyInit<SpinNoPreempt<Vec<EventSourceEntry>>> = LazyInit::new();

fn event_sources() -> &'static SpinNoPreempt<Vec<EventSourceEntry>> {
    EVENT_SOURCES.call_once(|| SpinNoPreempt::new(Vec::new()));
    #[expect(
        clippy::expect_used,
        reason = "CG-021: call_once above initializes the source table; failure is unreachable"
    )]
    EVENT_SOURCES.get().expect("event-source table initialized")
}

/// Registers a source so [`start_event_sources`] can start it.
///
/// Registration is permanent for the lifetime of the kernel. Registering the
/// same source twice is rejected: a second `start` would install the sink twice
/// and duplicate every hardware event.
pub fn register_event_source(source: &'static dyn HotplugEventSource) -> KResult<()> {
    let mut sources = event_sources().lock();
    if sources
        .iter()
        .any(|entry| core::ptr::eq(entry.source, source))
    {
        return Err(KErrorKind::AlreadyExists.into());
    }
    sources.push(EventSourceEntry {
        source,
        is_started: false,
    });
    Ok(())
}

/// Starts every registered source that has not started yet.
///
/// Returns the first error after attempting all sources. A source whose `start`
/// failed is left unstarted so a later call can retry; successful sources are
/// never started twice.
pub fn start_event_sources() -> KResult<()> {
    let pending: Vec<&'static dyn HotplugEventSource> = {
        let mut sources = event_sources().lock();
        let mut pending = Vec::new();
        for entry in sources.iter_mut() {
            if !entry.is_started {
                entry.is_started = true;
                pending.push(entry.source);
            }
        }
        pending
    };

    let mut first_error = None;
    for source in pending {
        if let Err(error) = source.start(&SINK) {
            if first_error.is_none() {
                first_error = Some(error);
            }
            let mut sources = event_sources().lock();
            if let Some(entry) = sources
                .iter_mut()
                .find(|entry| core::ptr::eq(entry.source, source))
            {
                entry.is_started = false;
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
