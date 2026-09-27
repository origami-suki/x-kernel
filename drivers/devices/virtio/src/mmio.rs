// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned static MMIO transport, transferable with its unique register mapping.
use virtio_drivers::{
    PhysAddr,
    transport::{
        DeviceStatus, DeviceType, InterruptStatus, Transport,
        mmio::MmioTransport as RawMmioTransport,
    },
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

/// Kernel-owned VirtIO MMIO register mapping returned only by the unsafe probe.
///
/// The locked upstream transport has unique MMIO pointers but lacks Send. This
/// concrete wrapper has no generic user state and exposes no inner references.
/// Its sole producer is probe_mmio_device, whose caller owns the entire static
/// device mapping. This does not make arbitrary Transport implementations Send.
pub struct MmioTransport {
    inner: RawMmioTransport<'static>,
}
impl MmioTransport {
    pub(crate) fn from_owned(inner: RawMmioTransport<'static>) -> Self {
        Self { inner }
    }
}
// SAFETY: concrete RawMmioTransport contains only unique static MMIO pointers
// and integer metadata, with no Rc/thread-local/user callback state. Moving
// the wrapper transfers that exclusive register owner; concurrent mutable
// accesses still require &mut or the driver's lock. The unsafe probe grants
// static MMIO ownership, not borrowed normal RAM or external mutable aliases.
unsafe impl Send for MmioTransport {}
impl Transport for MmioTransport {
    fn device_type(&self) -> DeviceType {
        self.inner.device_type()
    }

    fn read_device_features(&mut self) -> u64 {
        self.inner.read_device_features()
    }

    fn write_driver_features(&mut self, features: u64) {
        self.inner.write_driver_features(features)
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        self.inner.max_queue_size(queue)
    }

    fn notify(&mut self, queue: u16) {
        self.inner.notify(queue)
    }

    fn get_status(&self) -> DeviceStatus {
        self.inner.get_status()
    }

    fn set_status(&mut self, status: DeviceStatus) {
        self.inner.set_status(status)
    }

    fn set_guest_page_size(&mut self, size: u32) {
        self.inner.set_guest_page_size(size)
    }

    fn requires_legacy_layout(&self) -> bool {
        self.inner.requires_legacy_layout()
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver: PhysAddr,
        device: PhysAddr,
    ) {
        self.inner
            .queue_set(queue, size, descriptors, driver, device)
    }

    fn queue_unset(&mut self, queue: u16) {
        self.inner.queue_unset(queue)
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        self.inner.queue_used(queue)
    }

    fn ack_interrupt(&mut self) -> InterruptStatus {
        self.inner.ack_interrupt()
    }

    fn read_config_generation(&self) -> u32 {
        self.inner.read_config_generation()
    }

    fn read_config_space<T: FromBytes + IntoBytes>(
        &self,
        offset: usize,
    ) -> virtio_drivers::Result<T> {
        self.inner.read_config_space(offset)
    }

    fn write_config_space<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> virtio_drivers::Result {
        self.inner.write_config_space(offset, value)
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;
    #[def_test]
    fn actual_gpu_transports_are_send() {
        fn require_send<T: Send>() {}
        require_send::<super::MmioTransport>();
        require_send::<crate::PciTransport>();
    }
}
