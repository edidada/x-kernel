//! Pure x2APIC command checks: no privileged MSRs are accessed by these tests.

use super::*;
use crate::lapic::LocalApicMode;

fn runtime() -> RuntimeLocalApic {
    let mode = LocalApicMode::X2Apic;
    RuntimeLocalApic {
        regs: LocalApicRegisters::new(mode),
        mode,
        error_vector: 0xf2,
        ipi_destination_mode: IpiDestMode::Physical,
        is_icr_busy: AtomicBool::new(false),
        _not_send_sync: PhantomData,
    }
}

#[test]
fn x2apic_retains_full_width_destination_in_nmi_and_fixed_commands() {
    let apic = runtime();
    for destination in [0, 0x100, 0x1234_5678, u32::MAX] {
        let nmi = apic
            .encode_command(
                IpiCommand::Nmi,
                IpiTarget::Destination(destination),
            )
            .unwrap();
        assert_eq!((nmi >> 32) as u32, destination);
        assert_eq!(nmi as u32, 0x4400);
        let fixed = apic
            .encode_command(
                IpiCommand::Fixed(0xf3),
                IpiTarget::Destination(destination),
            )
            .unwrap();
        assert_eq!((fixed >> 32) as u32, destination);
        assert_eq!(fixed as u32, 0x40f3);
    }
}

#[test]
fn nested_x2apic_sender_is_rejected_before_any_privileged_register_access() {
    let apic = runtime();
    apic.is_icr_busy.store(true, Ordering::Relaxed);
    // SAFETY: software admission must reject before touching real MSRs. This
    // test deliberately runs in userspace, where an attempted WRMSR would fail.
    let result = unsafe {
        apic.try_send_ipi(IpiCommand::Nmi, IpiTarget::Destination(0x1234))
    };
    assert_eq!(result, Err(IpiSendError::Busy));
    assert!(apic.is_icr_busy.load(Ordering::Relaxed));
    {
        let _owner = IcrClaim(&apic.is_icr_busy);
    }
    assert!(!apic.is_icr_busy.load(Ordering::Relaxed));
}

#[test]
fn nmi_broadcast_encodes_shorthand_without_a_fake_interrupt_vector() {
    let apic = runtime();
    let command = apic
        .encode_command(IpiCommand::Nmi, IpiTarget::AllExcludingSelf)
        .unwrap();
    assert_eq!(command as u32 & 0xff, 0);
    assert_eq!(command as u32 & (7 << 8), 4 << 8);
    assert_eq!(command as u32 & (3 << 18), 3 << 18);
    assert_eq!(command >> 32, 0);
}
