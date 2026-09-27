// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Traditional virgl commands and host execution capabilities.
use alloc::{boxed::Box, sync::Arc};
use core::any::Any;

use crate::{DriverResult, ScanoutRect};

/// Owned, contiguous guest backing kept until fenced host teardown.
#[derive(Clone)]
pub struct GpuBacking {
    /// Device-visible physical address.
    address: u64,
    /// Number of accessible bytes.
    length: u32,
    /// Allocation owner; must not itself own the display device.
    owner: Arc<dyn Any + Send + Sync>,
}

impl GpuBacking {
    /// Device-visible start address.
    pub fn address(&self) -> u64 {
        self.address
    }

    /// Accessible byte length.
    pub fn length(&self) -> u32 {
        self.length
    }

    /// Allocation lifetime owner.
    pub fn owner(&self) -> &Arc<dyn Any + Send + Sync> {
        &self.owner
    }

    /// Creates an owned DMA range.
    ///
    /// # Safety
    /// `owner` must keep this entire device-accessible, coherent range alive.
    /// CPU access must obey the GPU reservation and transfer synchronization.
    /// The allocation must not move; it must not retain the display device.
    pub unsafe fn new(address: u64, length: u32, owner: Arc<dyn Any + Send + Sync>) -> Self {
        Self {
            address,
            length,
            owner,
        }
    }
}

/// Parameters of a traditional virgl resource (not a blob).
#[derive(Clone, Copy, Debug)]
pub struct VirglResourceCreate {
    /// Globally unique nonzero device resource ID.
    pub resource_id: u32,
    /// Gallium resource target.
    pub target: u32,
    /// Virgl format number.
    pub format: u32,
    /// Virgl bind flags.
    pub bind: u32,
    /// Base-level width.
    pub width: u32,
    /// Base-level height.
    pub height: u32,
    /// Base-level depth.
    pub depth: u32,
    /// Number of array elements.
    pub array_size: u32,
    /// Highest mip level.
    pub last_level: u32,
    /// Sample count (zero means not multisampled).
    pub nr_samples: u32,
    /// Resource creation flags.
    pub flags: u32,
}

/// A region in a resource's selected mip level.
#[derive(Clone, Copy, Debug)]
pub struct VirglBox {
    /// X origin.
    pub x: u32,
    /// Y origin.
    pub y: u32,
    /// Z origin.
    pub z: u32,
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Depth in texels.
    pub depth: u32,
}

/// Parameters for a fenced guest/host transfer.
#[derive(Clone, Copy, Debug)]
pub struct VirglTransfer {
    /// Rendering context ID.
    pub context_id: u32,
    /// Device resource ID (not a GEM handle).
    pub resource_id: u32,
    /// Region to transfer.
    pub box_: VirglBox,
    /// Offset into guest backing in bytes.
    pub offset: u64,
    /// Mip level.
    pub level: u32,
    /// Row stride; zero requests the protocol default.
    pub stride: u32,
    /// Layer stride; zero requests the protocol default.
    pub layer_stride: u32,
}

/// Synchronous traditional virgl commands. Success means fenced completion.
///
/// Call only from sleepable task context without MM or general DRM state locks.
/// Userspace callers additionally need a process-owned reservation and must keep
/// all referenced resources alive. No blob/context-init/explicit-fence ABI is
/// implied by this interface. Legacy contexts always use context_init = 0.
pub enum VirglCommand<'a> {
    /// Create a legacy context with a bounded diagnostic name.
    CreateContext { context_id: u32, name: &'a str },
    /// Destroy a context after its users have quiesced.
    DestroyContext { context_id: u32 },
    /// Attach an existing resource to a context.
    AttachContext { context_id: u32, resource_id: u32 },
    /// Detach an existing resource from a context.
    DetachContext { context_id: u32, resource_id: u32 },
    /// Create and attach owned guest backing; ownership transfers even on error.
    CreateResource {
        params: VirglResourceCreate,
        backing: GpuBacking,
    },
    /// Upload a resource region.
    TransferToHost(VirglTransfer),
    /// Read back a resource region. Success allows synchronized CPU reading.
    TransferFromHost(VirglTransfer),
    /// Submit kernel-owned command bytes copied from userspace beforehand.
    Submit { context_id: u32, commands: &'a [u8] },
    /// Present an existing host-rendered resource without uploading guest pages.
    Present { resource_id: u32, rect: ScanoutRect },
    /// Detach backing and unref; the allocation is released only after success.
    DestroyResource { resource_id: u32 },
}

/// Nonblocking diagnostics for admission failures and retained DMA ownership.
#[derive(Clone, Copy, Debug, Default)]
pub struct GpuCommandStatus {
    /// Tracked resources, including failed/pending teardown.
    pub resources: usize,
    /// Tracked contexts.
    pub contexts: usize,
    /// Objects marked for deferred teardown.
    pub pending_cleanup: usize,
    /// Requests whose DMA completion has not been consumed.
    pub pending_requests: usize,
    /// A timeout or malformed response has stopped new command admission.
    pub is_failed: bool,
}

/// Per-transaction host wait operations, used only outside queue spinlocks.
pub trait GpuWait {
    /// Monotonic nanoseconds for a bounded command deadline.
    fn now_ns(&self) -> u64;
    /// Yield execution so timer, peer callers and device workers can progress.
    fn yield_execution(&mut self);
}

/// Preallocated, nonblocking workqueue trigger. Scheduling never runs inline.
pub trait GpuDeferredWork: Send + Sync {
    /// Queue or coalesce work; failure must be reported without losing owners.
    fn schedule(&self) -> DriverResult;
}

/// Host-owned sleepable GPU execution gate and deferred-work factory.
pub trait GpuExecution: Send + Sync {
    /// Validate sleepability, serialize a transaction and run it with IRQs on.
    /// The callback must not recursively enter this execution gate.
    fn run(&self, operation: &mut dyn FnMut(&mut dyn GpuWait) -> DriverResult) -> DriverResult;
    /// Preallocate a task-context callback; must not execute it during creation.
    fn deferred(&self, callback: Box<dyn Fn() + Send + Sync>) -> Arc<dyn GpuDeferredWork>;
}
