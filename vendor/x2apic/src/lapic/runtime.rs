//! Shared CPU-local register access after exclusive APIC preparation.

use core::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};

use super::{lapic_msr::*, LocalApic, LocalApicMode};

/// Kind of CPU-local interrupt command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiCommand {
    /// Deliver a maskable interrupt on the given vector.
    Fixed(u8),
    /// Initialize a processor before startup.
    Init,
    /// Start a processor at the given physical 4-KiB page number.
    Startup(u8),
    /// Deliver a non-maskable interrupt; no vector is encoded.
    Nmi,
}

/// Hardware destination of an interrupt command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiTarget {
    /// Raw destination id in the startup-selected destination mode.
    Destination(u32),
    /// The executing processor.
    ThisCpu,
    /// Every processor except the executing one.
    AllExcludingSelf,
}

/// Rejection before an ICR command is submitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiSendError {
    /// Another local writer or xAPIC delivery currently owns the ICR.
    Busy,
    /// The destination cannot be represented in the selected APIC mode.
    InvalidDestination,
}

/// Immutable mode/register metadata with source-specific hardware operations.
///
/// All accesses target the CPU executing the instruction. This handle is
/// neither Send nor Sync. Its owner must pin every operation to the CPU on
/// which it was prepared. Shared references permit NMI reentry without an
/// overlapping mutable Rust borrow, but do not serialize hardware RMWs:
/// timer, error and performance-LVT operations require their own source owner.
/// ICR writers are serialized internally, including across NMI interruption.
#[derive(Debug)]
pub struct RuntimeLocalApic {
    regs: LocalApicRegisters,
    mode: LocalApicMode,
    error_vector: u32,
    ipi_destination_mode: IpiDestMode,
    is_icr_busy: AtomicBool,
    _not_send_sync: PhantomData<*const ()>,
}

impl RuntimeLocalApic {
    pub(super) fn new(apic: LocalApic) -> Self {
        Self {
            regs: apic.regs,
            mode: apic.mode,
            error_vector: apic.error_vector as u32,
            ipi_destination_mode: apic.ipi_destination_mode,
            is_icr_busy: AtomicBool::new(false),
            _not_send_sync: PhantomData,
        }
    }

    /// Programs the owned timer as one-shot with divide-by-one.
    ///
    /// # Safety
    /// The local APIC must be prepared on this pinned CPU. Exclude competing
    /// timer programming throughout this operation; the mapping must stay live.
    pub unsafe fn prepare_timer(&self) {
        // SAFETY: The caller exclusively owns local timer programming.
        unsafe {
            self.regs.set_lvt_timer_bit_range(
                LVT_TIMER_MODE,
                TimerMode::OneShot.into_u32(),
            )
        };
        // SAFETY: Same owned timer and live CPU-local register interface.
        unsafe {
            self.regs.set_tdcr_bit_range(
                TDCR_DIVIDE_VALUE,
                TimerDivide::Div1.into_u32(),
            )
        };
    }

    /// Changes only the owned timer's delivery mask.
    ///
    /// # Safety
    /// Retain the pinned, prepared CPU and exclude competing timer LVT writes.
    pub unsafe fn set_timer_enabled(&self, enabled: bool) {
        // SAFETY: The caller owns the local timer LVT RMW transaction.
        unsafe { self.regs.set_lvt_timer_bit(LVT_TIMER_MASK, !enabled) };
    }

    /// Starts the owned countdown, or stops it when `initial` is zero.
    ///
    /// # Safety
    /// Retain the prepared CPU and exclude competing initial-count writes.
    pub unsafe fn set_timer_initial(&self, initial: u32) {
        // SAFETY: The caller owns this CPU's timer countdown programming.
        unsafe { self.regs.write_ticr(initial) };
    }

    /// Completes exactly one in-service maskable interrupt.
    ///
    /// # Safety
    /// The caller owns that interrupt on this prepared, pinned CPU. Never use
    /// this for an NMI or spurious vector, and do not complete an event twice.
    pub unsafe fn end_of_interrupt(&self) {
        // SAFETY: Caller supplies the unique in-service completion obligation.
        unsafe { self.regs.write_eoi(0) };
    }

    /// Enables the prepared error vector, refreshing ESR around activation.
    ///
    /// # Safety
    /// Masked setup and error-handler registration must be complete on this
    /// pinned CPU. Exclude other ESR/error-LVT writers until this returns.
    pub unsafe fn enable_error(&self) {
        // SAFETY: The caller retains this prepared CPU's register mapping.
        let has_esr =
            unsafe { self.regs.version_bit_range(VERSION_MAX_LVT_ENTRY) } > 3;
        if has_esr {
            // SAFETY: This controller implements writable ESR; error source is owned.
            unsafe { self.regs.write_error(0) };
        }
        // SAFETY: Read refreshed error status on the owned CPU-local source.
        let _ = unsafe { self.regs.error() };
        // SAFETY: Startup validated the vector, whose handler is now installed.
        unsafe { self.regs.write_lvt_error(self.error_vector) };
        if has_esr {
            // SAFETY: Same exclusive error-source activation transaction.
            unsafe { self.regs.write_error(0) };
        }
        // SAFETY: Complete the activation refresh on the same CPU.
        let _ = unsafe { self.regs.error() };
    }

    /// Captures error status and completes an in-service error interrupt once.
    ///
    /// # Safety
    /// Retain the prepared CPU, own this error event and exclude competing
    /// ESR accesses. Do not call for the spurious vector or issue a second EOI.
    pub unsafe fn acknowledge_error(&self) -> ErrorFlags {
        // SAFETY: The caller supplies a live, prepared CPU-local interface.
        if unsafe { self.regs.version_bit_range(VERSION_MAX_LVT_ENTRY) } > 3 {
            // SAFETY: Supported ESR is exclusively owned by this error handler.
            unsafe { self.regs.write_error(0) };
        }
        // SAFETY: Capture the refreshed ESR before completing this delivery.
        let flags =
            ErrorFlags::from_bits_truncate(unsafe { self.regs.error() } as u8);
        // SAFETY: The caller owns this error's unique completion obligation.
        unsafe { self.end_of_interrupt() };
        flags
    }

    /// Routes the owned performance interrupt as an NMI, masked or enabled.
    /// Writes the complete LVT value, avoiding a task/NMI read-modify-write race.
    ///
    /// # Safety
    /// This prepared CPU must implement LVTPC and the caller must own its
    /// configuration and producer. Stop the producer before initial setup;
    /// establish source state before enabling. This does not stop a counter.
    pub unsafe fn set_performance_nmi_enabled(&self, enabled: bool) {
        let value = (4 << 8) | if enabled { 0 } else { 1 << 16 };
        // SAFETY: The source owner validated LVTPC support and owns this write.
        unsafe { self.regs.write_lvt_perf(value) };
    }

    /// Whether an interrupted CPU-local sender retains ICR admission.
    /// Only the owning CPU may inspect this; NMI code must never wait for it.
    pub fn has_icr_writer(&self) -> bool {
        self.is_icr_busy.load(Ordering::Acquire)
    }

    /// Whether xAPIC still reports an earlier hardware delivery in progress.
    ///
    /// # Safety
    /// Retain the initialized CPU and live register mapping. This reads only
    /// local delivery status and provides no remote completion guarantee.
    pub unsafe fn delivery_pending(&self) -> bool {
        if self.mode == LocalApicMode::X2Apic { return false; }
        // SAFETY: the caller supplies initialized CPU-local register access.
        unsafe { self.regs.icr_bit(ICR_DELIVERY_STATUS) }
    }

    /// Attempts one command without waiting for another writer or delivery.
    /// Busy/invalid-destination rejection performs no ICR writes. The software
    /// claim covers both halves of an xAPIC command, so a nested NMI cannot
    /// overwrite the interrupted sender's destination.
    ///
    /// # Safety
    /// Retain this prepared CPU with migration and ordinary IRQs excluded.
    /// Fixed vectors need installed handlers; INIT/SIPI targets and startup
    /// memory must be valid. NMI entry must be ready on every targeted CPU.
    pub unsafe fn try_send_ipi(
        &self,
        command: IpiCommand,
        target: IpiTarget,
    ) -> Result<(), IpiSendError> {
        let value = self.encode_command(command, target)?;
        if self
            .is_icr_busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(IpiSendError::Busy);
        }
        // Acquire/Release orders consecutive writers. More importantly the
        // retained admission excludes an NMI writer throughout the split xAPIC
        // destination/command writes; a nested attempt returns without spinning.
        let _claim = IcrClaim(&self.is_icr_busy);
        if matches!(self.mode, LocalApicMode::XApic { .. }) {
            // SAFETY: Caller retains the live CPU; this writer owns ICR admission.
            if unsafe { self.regs.icr_bit(ICR_DELIVERY_STATUS) } {
                return Err(IpiSendError::Busy);
            }
        }
        if self.mode == LocalApicMode::X2Apic {
            // x2APIC WRMSR is not serializing. Match Linux's
            // weak_wrmsr_fence: publish prior request stores before the command
            // and prevent WRMSR execution from passing their completion.
            // SAFETY: MFENCE/LFENCE are available on the x86_64 target, touch
            // no memory directly and preserve registers and flags.
            unsafe {
                core::arch::asm!(
                    "mfence",
                    "lfence",
                    options(nostack, preserves_flags)
                )
            };
        }
        // SAFETY: Valid encoded command, exclusive writer admission retained
        // through both xAPIC writes (or the single x2APIC MSR write).
        unsafe { self.regs.write_icr(value) };
        Ok(())
    }

    fn encode_command(
        &self,
        command: IpiCommand,
        target: IpiTarget,
    ) -> Result<u64, IpiSendError> {
        let (vector, delivery) = match command {
            IpiCommand::Fixed(vector) => (vector, 0),
            IpiCommand::Init => (0, 5),
            IpiCommand::Startup(page) => (page, 6),
            IpiCommand::Nmi => (0, 4),
        };
        let mut value = u64::from(vector) | (delivery << 8) | (1 << ICR_LEVEL);
        if self.ipi_destination_mode == IpiDestMode::Logical {
            value |= 1 << ICR_DESTINATION_MODE;
        }
        match target {
            IpiTarget::Destination(id) => {
                let destination = match self.mode {
                    LocalApicMode::X2Apic => id,
                    LocalApicMode::XApic { .. } if id <= u32::from(u8::MAX) => {
                        id << 24
                    }
                    LocalApicMode::XApic { .. } => {
                        return Err(IpiSendError::InvalidDestination)
                    }
                };
                value |= u64::from(destination) << 32;
            }
            IpiTarget::ThisCpu => value |= 1 << 18,
            IpiTarget::AllExcludingSelf => value |= 3 << 18,
        }
        Ok(value)
    }
}

struct IcrClaim<'a>(&'a AtomicBool);

impl Drop for IcrClaim<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
#[path = "../../tests/host/runtime.rs"]
mod runtime_tests;
