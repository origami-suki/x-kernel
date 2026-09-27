// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common traits and types for graphics display device drivers.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;

mod virgl;
#[doc(no_inline)]
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};
pub use virgl::*;

/// The information of the graphics display device.
#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    /// The visible width.
    pub width: u32,
    /// The visible height.
    pub height: u32,
}

/// A 2D scanout rectangle.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct ScanoutRect {
    /// The left coordinate in pixels.
    pub x: u32,
    /// The top coordinate in pixels.
    pub y: u32,
    /// The rectangle width in pixels.
    pub width: u32,
    /// The rectangle height in pixels.
    pub height: u32,
}

/// Pixel format used by scanout resources.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ScanoutFormat {
    /// 32-bit BGRA/XRGB memory layout as consumed by virtio-gpu 2D resources.
    Bgra8888,
}

/// A host-visible 2D resource backed by guest memory.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ScanoutResource {
    /// Driver-local resource identifier.
    pub id: u32,
    /// Resource width in pixels.
    pub width: u32,
    /// Resource height in pixels.
    pub height: u32,
    /// Resource stride in bytes.
    pub pitch: u32,
    /// Resource pixel format.
    pub format: ScanoutFormat,
}

/// A device-provided virgl capability set, independent of any DRM ABI.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct VirglCapsetInfo {
    /// Protocol capability-set identifier (1 for virgl, 2 for virgl2).
    pub id: u32,
    /// Highest queryable version, including version zero.
    pub max_version: u32,
    /// Maximum byte length of a capability-set response.
    pub max_size: u32,
}

/// Operations that require a graphics device driver to implement.
pub trait DisplayDevice: Device {
    /// Get the display information.
    fn info(&self) -> DisplayInfo;

    /// Whether need to flush the framebuffer to the screen.
    fn need_flush(&self) -> bool;

    /// Flush framebuffer to the screen.
    fn flush(&self) -> DriverResult;

    /// Whether the device negotiated the virgl protocol feature.
    ///
    /// This describes hardware capability, not availability of a rendering
    /// submission API or a userspace DRM render node. This query does not wait.
    fn has_virgl(&self) -> bool {
        false
    }

    /// Returns immutable, device-provided virgl capability-set metadata, or
    /// `None` when the protocol is unavailable.
    ///
    /// This query does not wait. An empty snapshot does not imply that a userspace
    /// rendering API is available, even when [`Self::has_virgl`] is true.
    fn virgl_capsets(&self) -> Option<Arc<[VirglCapsetInfo]>> {
        None
    }

    /// Returns the device-provided bytes for an advertised set and version.
    ///
    /// The immutable snapshot outlives the device if retained. This query does not
    /// wait. Drivers without virgl return [`DriverError::Unsupported`]; an
    /// unknown set or version returns [`DriverError::InvalidInput`]. Consumers
    /// must parse and validate the opaque protocol bytes before using them.
    fn virgl_capset(&self, _id: u32, _version: u32) -> DriverResult<Arc<[u8]>> {
        Err(DriverError::Unsupported)
    }

    /// Whether the traditional virgl command backend is implemented and operational.
    fn has_virgl_commands(&self) -> bool {
        false
    }

    /// Execute a typed command synchronously through a sleepable execution gate.
    /// On error the driver may retain DMA backing; callers must not infer cancellation.
    fn virgl_command(&self, _command: VirglCommand<'_>) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Mark a resource for deferred fenced teardown without waiting.
    /// No driver entry is allocated; the host scheduler may allocate bookkeeping.
    /// Safe from object Drop while MM locks are held; retained owners survive failure.
    fn enqueue_virgl_cleanup(&self, _resource_id: u32) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Mark a context for deferred teardown without waiting.
    fn enqueue_virgl_context_cleanup(&self, _context_id: u32) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Drain marked teardown in sleepable task context; retry after scheduling failure.
    /// Must never be called from object Drop or while holding MM/state locks.
    fn flush_virgl_cleanup(&self) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Return a nonblocking snapshot of owned objects and command failure state.
    fn virgl_command_status(&self) -> GpuCommandStatus {
        GpuCommandStatus::default()
    }

    /// Create a host-visible 2D resource backed by guest memory.
    fn create_scanout_resource(
        &self,
        _resource: ScanoutResource,
        _paddr: u64,
        _length: u32,
    ) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Create a 2D resource with owned backing retained through host teardown.
    /// Uses the same bounded, sleepable command lifecycle as virgl resources.
    fn create_scanout_resource_owned(
        &self,
        _resource: ScanoutResource,
        _backing: GpuBacking,
    ) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Destroy a previously created scanout resource.
    fn destroy_scanout_resource(&self, _resource_id: u32) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Disable the active scanout after synchronous fenced device completion.
    ///
    /// Requires sleepable task context. Success commits the disabled state;
    /// errors leave resource owners intact and do not imply DMA cancellation.
    /// Disabling scanout does not destroy the resource or its backing allocation.
    fn disable_scanout(&self) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Transfer resource contents to the host and make it the active scanout.
    fn present_scanout_resource(&self, _resource_id: u32, _rect: ScanoutRect) -> DriverResult {
        Err(DriverError::Unsupported)
    }
}

mod tests;
