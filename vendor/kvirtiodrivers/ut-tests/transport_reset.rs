use crate::transport::{
    DeviceStatus, SomeTransport, Transport,
    mmio::{LEGACY_VERSION, MODERN_VERSION, MmioTransport, VirtIOHeader},
};
use safe_mmio::UniqueMmioPointer;

#[test]
fn mmio_reset_through_wrapper_requires_status_readback() {
    for version in [LEGACY_VERSION, MODERN_VERSION] {
        let mut header = VirtIOHeader::make_fake_header(version, 4, 0, 0, 8);
        let transport = MmioTransport::new_from_unique(
            UniqueMmioPointer::from(&mut header),
            UniqueMmioPointer::from([].as_mut_slice()),
        )
        .unwrap();
        let mut transport = SomeTransport::from(transport);
        transport.set_status(DeviceStatus::DRIVER_OK);
        transport.begin_reset();
        assert!(transport.get_status().is_empty());
        transport.set_status(DeviceStatus::DRIVER_OK);
        assert!(!transport.poll_reset());
        transport.set_status(DeviceStatus::empty());
        assert!(transport.poll_reset());
        assert!(transport.poll_reset());
    }
}
