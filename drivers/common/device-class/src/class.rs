// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Family-sentinel tests: the aggregate remove_all_detached drain and the
//! MMIO lease model, both of which need concrete family runtimes. The
//! generic class-machinery tests moved to `driver-base::class::tests`.

use kerrno::KError;
use unittest::def_test;

use super::*;

#[def_test(serial)]
fn remove_all_detached_drains_every_enabled_class() {
    use crate::remove_all_detached;

    fn loc(id: u32) -> alloc::string::String {
        alloc::format!("platform/static{id}")
    }

    let owner: u64 = 0xfeed_beef;
    let mut published = 0usize;

    #[cfg(feature = "hwrng")]
    {
        struct SentinelHwrng;
        impl subsys_hwrng::HwrngDevice for SentinelHwrng {
            fn read(&self, _buf: &mut [u8]) -> KResult<usize> {
                Ok(0)
            }

            fn is_failed(&self) -> bool {
                false
            }
        }
        crate::publish_hwrng(
            ClassParent::detached(owner, "drain-test", loc(1)),
            alloc::sync::Arc::new(SentinelHwrng),
            "drain-sentinel",
        )
        .unwrap();
        published += 1;
    }

    #[cfg(feature = "net")]
    {
        struct SentinelNet;
        impl subsys_net::NetDevice for SentinelNet {
            fn mac(&self) -> subsys_net::MacAddress {
                subsys_net::MacAddress([0; 6])
            }

            fn can_tx(&self) -> bool {
                false
            }

            fn can_rx(&self) -> bool {
                false
            }

            fn rx_queue_len(&self) -> usize {
                0
            }

            fn tx_queue_len(&self) -> usize {
                0
            }

            fn recycle_rx(&self, _rx_buf: subsys_net::NetBufHandle) -> KResult {
                Ok(())
            }

            fn recycle_tx(&self) -> KResult {
                Ok(())
            }

            fn send(&self, _tx_buf: subsys_net::NetBufHandle) -> KResult {
                Err(KError::Unsupported)
            }

            fn recv(&self) -> KResult<subsys_net::NetBufHandle> {
                Err(KError::Unsupported)
            }

            fn alloc_tx_buf(&self, _size: usize) -> KResult<subsys_net::NetBufHandle> {
                Err(KError::Unsupported)
            }
        }
        crate::publish_net(
            ClassParent::detached(owner, "drain-test", loc(3)),
            alloc::boxed::Box::new(SentinelNet),
            "drain-sentinel",
        )
        .unwrap();
        published += 1;
    }

    assert!(published > 0, "no class features enabled in this build");
    remove_all_detached(owner);

    macro_rules! assert_drained {
        ($feature:literal, $registry:path) => {
            #[cfg(feature = $feature)]
            assert!($registry().lock().find_detached(owner).is_none());
        };
    }
    assert_drained!("hwrng", crate::hwrng_class_registry);
    assert_drained!("net", crate::net_class_registry);
}

#[def_test(serial)]
fn unpublish_keeps_consumer_clone_leases_alive() {
    use core::sync::atomic::{AtomicBool, Ordering};

    use crate::NetDevice;

    struct LeaseOnDrop(Arc<AtomicBool>);
    impl Drop for LeaseOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    struct SentinelNet {
        _lease: LeaseOnDrop,
    }
    impl NetDevice for SentinelNet {
        fn mac(&self) -> subsys_net::MacAddress {
            subsys_net::MacAddress([0; 6])
        }

        fn can_tx(&self) -> bool {
            false
        }

        fn can_rx(&self) -> bool {
            false
        }

        fn rx_queue_len(&self) -> usize {
            0
        }

        fn tx_queue_len(&self) -> usize {
            0
        }

        fn recycle_rx(&self, _rx_buf: subsys_net::NetBufHandle) -> KResult {
            Ok(())
        }

        fn recycle_tx(&self) -> KResult {
            Ok(())
        }

        fn send(&self, _tx_buf: subsys_net::NetBufHandle) -> KResult {
            Err(KError::Unsupported)
        }

        fn recv(&self) -> KResult<subsys_net::NetBufHandle> {
            Err(KError::Unsupported)
        }

        fn alloc_tx_buf(&self, _size: usize) -> KResult<subsys_net::NetBufHandle> {
            Err(KError::Unsupported)
        }

        fn set_rx_scheduler(
            &self,
            _scheduler: Option<Arc<dyn subsys_net::NetRxScheduler>>,
        ) -> KResult {
            Err(KError::Unsupported)
        }
    }

    fn loc2(id: u32) -> alloc::string::String {
        alloc::format!("platform/firmware{id}")
    }

    let released = Arc::new(AtomicBool::new(false));
    let runtime: crate::NetDeviceImpl = alloc::boxed::Box::new(SentinelNet {
        _lease: LeaseOnDrop(released.clone()),
    });
    let publication = crate::publish_net(
        ClassParent::detached(0x1eaf, "lease-test", loc2(0x1eaf)),
        runtime,
        "lease-sentinel",
    )
    .unwrap();

    // A consumer holds a clone across the device's removal — the exact
    // window the old Data-scoped mapping made a dangling pointer.
    let consumer = publication.clone();

    // The activation's close: unpublish by publication id.
    crate::unpublish_net(publication.id());
    drop(publication);

    // The mapping is NOT released with the publication (nor with the
    // driver Data): the clone's runtime still owns it.
    assert!(!released.load(Ordering::Acquire));

    // Data-plane admission is closed — netif_tx_disable parity: the
    // poll loop sees a stopped queue instead of retrying errors.
    assert!(!consumer.can_tx());
    assert!(!consumer.can_rx());
    // Observation still resolves valid memory (the runtime is alive);
    // nothing dangles.
    let _mac = consumer.mac();

    // The LAST reference drops — only now does the mapping release
    // (pci_release_regions after the last put_device).
    drop(consumer);
    assert!(released.load(Ordering::Acquire));
}
