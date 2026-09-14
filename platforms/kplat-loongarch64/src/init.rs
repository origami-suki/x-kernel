// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kbuild_config::RTC_PADDR;
use khal::mem::PhysAddr;
use kplat::boot::{BootHandler, BootInfo};

/// Size of the LS7A RTC MMIO aperture.
const LS7A_RTC_SIZE: usize = 0x1000;

#[kiface::provide]
impl BootHandler {
    fn prepare_boot_memory(_boot_info: &BootInfo) {}

    fn firmware_init(_boot_info: &BootInfo) {}

    fn early_driver_init() {
        crate::time::early_init();
        crate::irq::init();
        console_driver::init_stdout_from_device_tree()
            .expect("failed to parse console from device tree");
        console_driver::register_input_irq_handler();
        #[cfg(feature = "rtc")]
        {
            // The LS7A RTC sits at the static `RTC_PADDR` aperture; map it and
            // sample a year-consistent wall clock via the shared `rtc_driver`
            // LS7A backend, then seed the realtime clock.
            let vaddr =
                memspace::iomap_device(PhysAddr::from_usize(RTC_PADDR), LS7A_RTC_SIZE, "ls7a-rtc")
                    .unwrap_or_else(|err| panic!("failed to iomap ls7a rtc: {err:?}"));
            let config = rtc_driver::RtcConfig::mmio_mapped(
                rtc_driver::RtcKind::Ls7a,
                vaddr,
                rtc_driver::RtcSource::PlatformStatic,
            );
            let sample = rtc_driver::read(config).expect("failed to read ls7a rtc");
            ktime::initialize_realtime(sample);
        }
    }

    fn final_init(_boot_info: &BootInfo) {
        crate::time::init_percpu();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_logical_cpu_id: kcpu_id_map::LogicalCpuId) {
        crate::time::init_percpu();
    }
}
