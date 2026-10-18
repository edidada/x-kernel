// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime console subsystem.
//!
//! The boot console remains a boot-time special path. Once the already active
//! boot console is adopted into the driver core, its runtime wrapper is
//! published through the generic char class. This module keeps a filtered local
//! console view and the adopted stdout identity, without a public console
//! selection mechanism.

use alloc::{sync::Arc, vec::Vec};

use driver_base::{Publication, PublicationId};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;
use subsys_char::{CharDeviceImpl, class::CHAR};

struct ConsoleSubsystem {
    devices: Vec<Publication<CharDeviceImpl>>,
    /// The adopted stdout publication. Console OWNS it: a rebind drops it,
    /// which withdraws the previous identity (projection included).
    stdout: Option<Publication<CharDeviceImpl>>,
    /// The adopted stdout's transferred guards (see [`StdoutBinding`]).
    stdout_binding: Option<StdoutBinding>,
}

impl ConsoleSubsystem {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            stdout: None,
            stdout_binding: None,
        }
    }

    fn add(&mut self, handle: Publication<CharDeviceImpl>) {
        let id = handle.id();
        if self.devices.iter().any(|current| current.id() == id) {
            return;
        }
        log::debug!(
            "console subsystem: added {:?} (driver={})",
            id,
            handle.driver_name().unwrap_or("<none>")
        );
        self.devices.push(handle);
    }

    fn remove(&mut self, id: PublicationId) -> Option<Publication<CharDeviceImpl>> {
        // Take the owning stdout publication out (never drop it under the
        // console lock: its withdrawal runs callbacks that re-enter here).
        let owned = if self.stdout.as_ref().is_some_and(|p| p.id() == id) {
            self.stdout.take()
        } else {
            None
        };
        let listed = self
            .devices
            .iter()
            .position(|device| device.id() == id)
            .map(|pos| self.devices.swap_remove(pos));
        owned.or(listed)
    }

    fn stdout_handle(&self) -> Option<Publication<CharDeviceImpl>> {
        let stdout = self.stdout.as_ref()?;
        self.devices
            .iter()
            .find(|device| device.id() == stdout.id())
            .cloned()
    }
}

static CONSOLE_SUBSYS: LazyInit<SpinNoPreempt<ConsoleSubsystem>> = LazyInit::new();
static CONSOLE_CLASS_BRIDGE: LazyInit<()> = LazyInit::new();

/// The adopted stdout binding: THE explicit cross-binding owner of the
/// boot console — publication identity, input IRQ, and (for cold-start
/// ports) the register mapping, held for the KERNEL's lifetime.
///
/// This is the console-continuity policy as ONE type instead of a
/// special case leaking through the driver: the serial driver's stdout
/// probe TRANSFERS its guards here (`adopt_stdout_guards`) and keeps no
/// withdrawal guard in its Data — the binding's entire runtime instance
/// survives driver removal together, or it does not survive at all (a
/// publication outliving its IRQ would read as an available console
/// that can never receive input).
///
/// A same-port rebind REPLACES the binding: the previous publication is
/// withdrawn (see `register_stdout_console`) and the previous guards
/// release with this assignment.
#[expect(
    dead_code,
    reason = "CG-024-lint-exceptions: ownership-only fields — the guards exist               for \
              their Drop (released at rebind replacement or subsystem               teardown), \
              never for reads"
)]
struct StdoutBinding {
    irq: Option<driver_base::DevIrq>,
    regs: Option<driver_base::IoMap>,
}

/// Transfer the adopted stdout port's binding guards to the console
/// subsystem (call from a successful stdout probe, AFTER publication).
/// Takes ownership: the caller's `Data` keeps `None` for these fields.
pub fn adopt_stdout_guards(irq: Option<driver_base::DevIrq>, regs: Option<driver_base::IoMap>) {
    // Replace: releasing the previous pair handles the rebind case (the
    // old registration/handlers belong to the superseded binding).
    console_subsystem().lock().stdout_binding = Some(StdoutBinding { irq, regs });
}

fn console_subsystem() -> &'static SpinNoPreempt<ConsoleSubsystem> {
    CONSOLE_SUBSYS.call_once(|| SpinNoPreempt::new(ConsoleSubsystem::new()));
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once completed successfully and \
                  permanently published the console subsystem"
    )]
    CONSOLE_SUBSYS
        .get()
        .expect("console subsystem call_once succeeded")
}

fn is_runtime_console(handle: &Publication<CharDeviceImpl>) -> bool {
    // Serial stdout is matched separately by its registered device ID, never
    // by driver name: auxiliary UARTs use the same serial driver.
    handle.driver_name() == Some("console")
}

fn add_console_device(handle: Publication<CharDeviceImpl>) {
    let is_console = is_runtime_console(&handle);
    let mut subsystem = console_subsystem().lock();
    let is_adopted_stdout = subsystem
        .stdout
        .as_ref()
        .is_some_and(|stdout| stdout.id() == handle.id());
    if !is_console && !is_adopted_stdout {
        return;
    }
    subsystem.add(handle);
}

fn ensure_console_bridge() {
    CONSOLE_CLASS_BRIDGE.call_once(|| {
        for handle in CHAR.devices() {
            add_console_device(handle);
        }
        CHAR.subscribe_available(Arc::new(add_console_device));
        // Class-layer removal subscription replaces the old device-model
        // removal event: the console keeps no stale handles.
        CHAR.subscribe_removed(Arc::new(|handle| {
            let _ = unregister_console_device(handle.id());
        }));
    });
}

/// Publish the adopted boot stdout device and remember its runtime identity.
///
/// Only the caller's device is adopted as a console — the serial driver uses
/// this for its stdout port. Auxiliary serial ports are published via
/// `publish_char` and ignored by the char-availability bridge, so they
/// cannot replace the stdout association.
/// The runtime implementation must wrap the same early stdout port, not another
/// device. Called by serial adoption in task context, before driver activation.
pub fn register_stdout_console(publication: Publication<CharDeviceImpl>) {
    ensure_console_bridge();
    let id = publication.id();
    // Rebind: dropping the previous stdout publication withdraws the old
    // identity (projection, callbacks, class teardown) before the new one
    // becomes THE console — one stdout identity, one publication.
    let previous = {
        let mut subsystem = console_subsystem().lock();
        let previous = subsystem.stdout.replace(publication);
        subsystem.devices.retain(|device| device.id() != id);
        previous
    };
    drop(previous);
    // This runs inside the driver's probe, so the device is usually not
    // active yet and `CHAR.find()` (which filters on availability) cannot
    // see it. Adopt immediately when already available.
    if let Some(handle) = CHAR.find(id) {
        console_subsystem().lock().add(handle);
    }
}

/// Snapshot the available runtime device associated with boot stdout.
///
/// Task context. Returns none until the device is published and active. The
/// handle retains ownership, not availability: callers must check availability
/// before later I/O. This does not provide synchronization for hot-unplug.
pub fn stdout_console() -> Option<Publication<CharDeviceImpl>> {
    let handle = console_subsystem().lock().stdout_handle()?;
    handle.is_available().then_some(handle)
}

/// Identity of the adopted boot stdout.
///
/// Task context. Available after publication, including before activation;
/// cleared on removal. This identifies the port, not its I/O availability.
pub fn stdout_console_id() -> Option<PublicationId> {
    console_subsystem()
        .lock()
        .stdout
        .as_ref()
        .map(Publication::id)
}

/// Remove a runtime console from the console subsystem.
pub fn unregister_console_device(id: PublicationId) -> Option<Publication<CharDeviceImpl>> {
    console_subsystem().lock().remove(id)
}

unittest::path_mod_test!(runtime_unittest, "../ut-tests/runtime_unittest.rs");
