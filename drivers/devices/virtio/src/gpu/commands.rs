// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned depth-two transactions. The host gate sleeps; queue locks never wait.
use alloc::collections::{BTreeMap, BTreeSet};
use core::mem::ManuallyDrop;

use display::{
    GpuBacking, GpuCommandStatus, GpuDeferredWork, GpuExecution, GpuWait, VirglCommand,
    VirglResourceCreate, VirglTransfer,
};

use super::*;

const COMMAND_TIMEOUT_NS: u64 = 30_000_000_000;
const MAX_RESOURCES: usize = 4096;
const MAX_CONTEXTS: usize = 1024;
const MAX_COMMAND_BYTES: usize = 16 * 1024 * 1024;
const FENCE: u32 = 1;

struct Pending<H: GpuHal> {
    transaction: Transaction<H>,
    fence: u64,
}
struct Reply {
    response: Vec<u8>,
    fence: u64,
}

struct Resource {
    backing: Option<GpuBacking>,
    contexts: BTreeSet<u32>,
    cleanup: bool,
    attached: bool,
    created: bool,
    is_3d: bool,
}

struct Context {
    cleanup: bool,
}

struct State<H: GpuHal, T: Transport + Send> {
    // Drop must observe reset acknowledgement before releasing DMA allocations.
    protocol: ManuallyDrop<VirtIoGpuProtocol<H, T>>,
    pending: Option<Pending<H>>,
    resources: BTreeMap<u32, Resource>,
    contexts: BTreeMap<u32, Context>,
    next_fence: u64,
    is_failed: bool,
    active_scanout: u32,
}

pub(super) struct Engine<H: GpuHal, T: Transport + Send> {
    state: SpinNoIrq<State<H, T>>,
    execution: Option<Arc<dyn GpuExecution>>,
    cleanup_work: Option<Arc<dyn GpuDeferredWork>>,
}

impl<H: GpuHal + 'static, T: Transport + Send + 'static> Engine<H, T> {
    pub(super) fn new(
        protocol: VirtIoGpuProtocol<H, T>,
        execution: Option<Arc<dyn GpuExecution>>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak: &alloc::sync::Weak<Self>| {
            let cleanup_work = execution.as_ref().map(|runtime| {
                let weak = weak.clone();
                runtime.deferred(Box::new(move || {
                    if let Some(engine) = weak.upgrade()
                        && let Err(error) = engine.reclaim()
                    {
                        log::warn!("virtio-gpu: deferred cleanup retained owners: {:?}", error);
                    }
                }))
            });
            Self {
                state: SpinNoIrq::new(State {
                    protocol: ManuallyDrop::new(protocol),
                    pending: None,
                    resources: BTreeMap::new(),
                    contexts: BTreeMap::new(),
                    next_fence: 1,
                    is_failed: false,
                    active_scanout: 0,
                }),
                execution,
                cleanup_work,
            }
        })
    }

    pub(super) fn status(&self) -> GpuCommandStatus {
        let state = self.state.lock();
        GpuCommandStatus {
            resources: state.resources.len(),
            contexts: state.contexts.len(),
            pending_cleanup: state.resources.values().filter(|r| r.cleanup).count()
                + state.contexts.values().filter(|c| c.cleanup).count(),
            pending_requests: usize::from(state.pending.is_some()),
            is_failed: state.is_failed,
        }
    }

    pub(super) fn available(&self) -> bool {
        self.execution.is_some() && !self.status().is_failed
    }

    fn run(&self, operation: &mut dyn FnMut(&mut dyn GpuWait) -> DriverResult) -> DriverResult {
        self.execution
            .as_ref()
            .ok_or(DriverError::Unsupported)?
            .run(&mut |wait| {
                // Admission applies to owned 2D creation as well as virgl. In
                // particular, rejected calls must not register fresh backing
                // after an earlier command made device state uncertain.
                if self.status().is_failed {
                    let _ = self.poll();
                    return Err(DriverError::BadState);
                }
                operation(wait)
            })
    }

    fn poison(&self) {
        self.state.lock().is_failed = true;
    }

    /// Consume queue state under the short lock, then retire streaming maps
    /// (including bounce copies) outside it.
    fn poll(&self) -> DriverResult<Option<Reply>> {
        let (pending, length) = {
            let mut state = self.state.lock();
            if state.pending.is_none() {
                return Ok(None);
            }
            match state.protocol.control_queue.pop() {
                Ok(Some(length)) => (state.pending.take().unwrap(), length),
                Ok(None) => return Ok(None),
                Err(_) => {
                    state.is_failed = true;
                    return Err(DriverError::Io);
                }
            }
        };
        // SAFETY: pop returned the sole chain owned by this pending transaction.
        let response = unsafe { pending.transaction.finish(length) }.map_err(|_| {
            self.poison();
            DriverError::Io
        })?;
        if response.len() != 24 {
            self.poison();
            return Err(DriverError::Io);
        }
        Ok(Some(Reply {
            response,
            fence: pending.fence,
        }))
    }

    fn request(&self, mut request: Vec<u8>, wait: &mut dyn GpuWait) -> DriverResult {
        let deadline = wait.now_ns().saturating_add(COMMAND_TIMEOUT_NS);
        let fence = {
            let mut state = self.state.lock();
            if state.is_failed || state.pending.is_some() {
                return Err(DriverError::BadState);
            }
            let fence = state.next_fence;
            state.next_fence = fence.checked_add(1).ok_or(DriverError::BadState)?;
            fence
        };
        request[4..8].copy_from_slice(&FENCE.to_le_bytes());
        request[8..16].copy_from_slice(&fence.to_le_bytes());
        // All fallible maps and large bounce copies happen with IRQs enabled,
        // before any descriptor or available index is written.
        let mut transaction = Transaction::<H>::prepare(request, 24).map_err(|error| {
            if error == virtio_drivers::Error::DmaError {
                DriverError::NoMemory
            } else {
                crate::as_driver_error(error)
            }
        })?;
        {
            let mut state = self.state.lock();
            if state.is_failed || state.pending.is_some() {
                return Err(DriverError::BadState);
            }
            state
                .protocol
                .control_queue
                .publish(&mut transaction)
                .map_err(crate::as_driver_error)?;
            state.pending = Some(Pending { transaction, fence });
            state.protocol.transport.notify(QUEUE_TRANSMIT);
        }
        loop {
            if let Some(pending) = self.poll()? {
                let response = &pending.response;
                let ty = u32::from_le_bytes(response[0..4].try_into().unwrap());
                let flags = u32::from_le_bytes(response[4..8].try_into().unwrap());
                let fence = u64::from_le_bytes(response[8..16].try_into().unwrap());
                if flags & FENCE == 0 || fence != pending.fence {
                    self.poison();
                    return Err(DriverError::Io);
                }
                return match ty {
                    0x1100 => Ok(()),
                    0x1201 => Err(DriverError::NoMemory),
                    0x1200 | 0x1202..=0x1205 => Err(DriverError::Io),
                    _ => {
                        self.poison();
                        Err(DriverError::Io)
                    }
                };
            }
            if wait.now_ns() >= deadline {
                self.poison();
                log::error!(
                    "virtio-gpu: command deadline exceeded; pending DMA and resource owners \
                     retained"
                );
                return Err(DriverError::Io);
            }
            wait.yield_execution();
        }
    }

    fn context(&self, id: u32) -> DriverResult {
        self.state
            .lock()
            .contexts
            .contains_key(&id)
            .then_some(())
            .ok_or(DriverError::InvalidInput)
    }

    fn resource(&self, id: u32) -> DriverResult {
        self.state
            .lock()
            .resources
            .contains_key(&id)
            .then_some(())
            .ok_or(DriverError::InvalidInput)
    }

    fn attach_context(&self, context: u32, resource: u32, wait: &mut dyn GpuWait) -> DriverResult {
        self.context(context)?;
        self.resource(resource)?;
        if self.state.lock().resources[&resource]
            .contexts
            .contains(&context)
        {
            return Ok(());
        }
        // Record before sending: timeout leaves an uncertain host attachment.
        self.state
            .lock()
            .resources
            .get_mut(&resource)
            .unwrap()
            .contexts
            .insert(context);
        let result = self.request(words(0x202, context, &[resource, 0]), wait);
        if result.is_err() && !self.status().is_failed {
            self.state
                .lock()
                .resources
                .get_mut(&resource)
                .unwrap()
                .contexts
                .remove(&context);
        }
        result
    }

    fn detach_context(&self, context: u32, resource: u32, wait: &mut dyn GpuWait) -> DriverResult {
        self.context(context)?;
        self.resource(resource)?;
        if !self.state.lock().resources[&resource]
            .contexts
            .contains(&context)
        {
            return Ok(());
        }
        self.request(words(0x203, context, &[resource, 0]), wait)?;
        self.state
            .lock()
            .resources
            .get_mut(&resource)
            .unwrap()
            .contexts
            .remove(&context);
        Ok(())
    }

    fn destroy_context(&self, context: u32, wait: &mut dyn GpuWait) -> DriverResult {
        self.context(context)?;
        self.request(words(0x201, context, &[]), wait)?;
        let mut state = self.state.lock();
        state.contexts.remove(&context);
        for resource in state.resources.values_mut() {
            resource.contexts.remove(&context);
        }
        Ok(())
    }

    fn destroy_resource(&self, id: u32, wait: &mut dyn GpuWait) -> DriverResult {
        self.resource(id)?;
        if self.state.lock().active_scanout == id {
            self.disable_scanout_locked(wait)?;
        }
        loop {
            let context = self.state.lock().resources[&id].contexts.first().copied();
            match context {
                Some(ctx) => self.detach_context(ctx, id, wait)?,
                None => break,
            }
        }
        if self.state.lock().resources[&id].attached {
            self.request(words(0x107, 0, &[id, 0]), wait)?;
            self.state.lock().resources.get_mut(&id).unwrap().attached = false;
        }
        if self.state.lock().resources[&id].created {
            self.request(words(0x102, 0, &[id, 0]), wait)?;
        }
        // Drop arbitrary backing owners only after leaving the state spinlock.
        let retired = self.state.lock().resources.remove(&id);
        drop(retired);
        Ok(())
    }

    fn create_resource(
        &self,
        params: VirglResourceCreate,
        backing: GpuBacking,
        wait: &mut dyn GpuWait,
    ) -> DriverResult {
        if params.resource_id == 0
            || params.width == 0
            || params.height == 0
            || params.depth == 0
            || (params.array_size == 0 && params.target != 0)
            || params.last_level > 31
            || backing.length() == 0
            || backing
                .address()
                .checked_add(u64::from(backing.length()))
                .is_none()
        {
            return Err(DriverError::InvalidInput);
        }
        let id = params.resource_id;
        let create = words(
            0x204,
            0,
            &[
                id,
                params.target,
                params.format,
                params.bind,
                params.width,
                params.height,
                params.depth,
                params.array_size,
                params.last_level,
                params.nr_samples,
                params.flags,
                0,
            ],
        );
        self.create_owned_resource(id, backing, true, create, wait)
    }

    fn create_owned_resource(
        &self,
        id: u32,
        backing: GpuBacking,
        is_3d: bool,
        create: Vec<u8>,
        wait: &mut dyn GpuWait,
    ) -> DriverResult {
        if backing
            .address()
            .checked_add(u64::from(backing.length()))
            .is_none()
        {
            return Err(DriverError::InvalidInput);
        }

        let address = backing.address();
        let length = backing.length();
        {
            let mut state = self.state.lock();
            if state.resources.contains_key(&id) {
                return Err(DriverError::AlreadyExists);
            }
            if state.resources.len() >= MAX_RESOURCES {
                return Err(DriverError::NoMemory);
            }
            state.resources.insert(
                id,
                Resource {
                    backing: Some(backing),
                    contexts: BTreeSet::new(),
                    cleanup: false,
                    attached: false,
                    created: true,
                    is_3d,
                },
            );
        }
        let result = (|| {
            let create_result = self.request(create, wait);
            if let Err(error) = create_result {
                if !self.status().is_failed {
                    self.state.lock().resources.get_mut(&id).unwrap().created = false;
                }
                return Err(error);
            }
            // Retain ownership before publishing an attach that may time out.
            self.state.lock().resources.get_mut(&id).unwrap().attached = true;
            let attach_result = self.request(
                words(
                    0x106,
                    0,
                    &[id, 1, address as u32, (address >> 32) as u32, length, 0],
                ),
                wait,
            );
            if attach_result.is_err() && !self.status().is_failed {
                self.state.lock().resources.get_mut(&id).unwrap().attached = false;
            }
            attach_result
        })();
        if result.is_err() {
            self.state.lock().resources.get_mut(&id).unwrap().cleanup = true;
            if !self.status().is_failed {
                let _ = self.destroy_resource(id, wait);
            }
        }
        result
    }

    fn transfer(
        &self,
        to_host: bool,
        params: VirglTransfer,
        wait: &mut dyn GpuWait,
    ) -> DriverResult {
        self.context(params.context_id)?;
        self.resource(params.resource_id)?;
        let b = params.box_;
        if b.width == 0
            || b.height == 0
            || b.depth == 0
            || b.x.checked_add(b.width).is_none()
            || b.y.checked_add(b.height).is_none()
            || b.z.checked_add(b.depth).is_none()
            || params.level > 31
        {
            return Err(DriverError::InvalidInput);
        }
        {
            let state = self.state.lock();
            let resource = &state.resources[&params.resource_id];
            if !resource.is_3d
                || !resource.contexts.contains(&params.context_id)
                || params.offset
                    >= u64::from(
                        resource
                            .backing
                            .as_ref()
                            .ok_or(DriverError::InvalidInput)?
                            .length(),
                    )
            {
                return Err(DriverError::InvalidInput);
            }
        }
        self.request(
            words(
                if to_host { 0x205 } else { 0x206 },
                params.context_id,
                &[
                    b.x,
                    b.y,
                    b.z,
                    b.width,
                    b.height,
                    b.depth,
                    params.offset as u32,
                    (params.offset >> 32) as u32,
                    params.resource_id,
                    params.level,
                    params.stride,
                    params.layer_stride,
                ],
            ),
            wait,
        )
    }

    pub(super) fn command(&self, command: VirglCommand<'_>) -> DriverResult {
        let mut command = Some(command);
        self.run(&mut |wait| {
            // A poisoned device may reclaim a late used descriptor, but cannot
            // replay an operation whose host outcome the caller could not know.
            if self.status().is_failed {
                let _ = self.poll();
                return Err(DriverError::BadState);
            }
            match command.take().ok_or(DriverError::BadState)? {
                VirglCommand::CreateContext { context_id, name } => {
                    if context_id == 0 || name.len() > 64 {
                        return Err(DriverError::InvalidInput);
                    }
                    {
                        let mut state = self.state.lock();
                        if state.contexts.contains_key(&context_id) {
                            return Err(DriverError::AlreadyExists);
                        }
                        if state.contexts.len() >= MAX_CONTEXTS {
                            return Err(DriverError::NoMemory);
                        }
                        state
                            .contexts
                            .insert(context_id, Context { cleanup: false });
                    }
                    let mut bytes = words(0x200, context_id, &[name.len() as u32, 0]);
                    bytes.extend_from_slice(name.as_bytes());
                    bytes.resize(96, 0);
                    let result = self.request(bytes, wait);
                    if result.is_err() && !self.status().is_failed {
                        self.state.lock().contexts.remove(&context_id);
                    } else if result.is_err() {
                        self.state
                            .lock()
                            .contexts
                            .get_mut(&context_id)
                            .unwrap()
                            .cleanup = true;
                    }
                    result
                }
                VirglCommand::DestroyContext { context_id } => {
                    self.destroy_context(context_id, wait)
                }
                VirglCommand::AttachContext {
                    context_id,
                    resource_id,
                } => self.attach_context(context_id, resource_id, wait),
                VirglCommand::DetachContext {
                    context_id,
                    resource_id,
                } => self.detach_context(context_id, resource_id, wait),
                VirglCommand::CreateResource { params, backing } => {
                    self.create_resource(params, backing, wait)
                }
                VirglCommand::TransferToHost(params) => self.transfer(true, params, wait),
                VirglCommand::TransferFromHost(params) => self.transfer(false, params, wait),
                VirglCommand::Submit {
                    context_id,
                    commands,
                } => {
                    self.context(context_id)?;
                    if !commands.len().is_multiple_of(4) || commands.len() > MAX_COMMAND_BYTES {
                        return Err(DriverError::InvalidInput);
                    }
                    let mut bytes = words(0x207, context_id, &[commands.len() as u32, 0]);
                    bytes.extend_from_slice(commands);
                    self.request(bytes, wait)
                }
                VirglCommand::Present { resource_id, rect } => {
                    self.resource(resource_id)?;
                    if !self.state.lock().resources[&resource_id].is_3d {
                        return Err(DriverError::InvalidInput);
                    }
                    self.present(resource_id, rect, false, wait)
                }
                VirglCommand::DestroyResource { resource_id } => {
                    self.destroy_resource(resource_id, wait)
                }
            }
        })
    }

    pub(super) fn enqueue(&self, id: u32, context: bool) -> DriverResult {
        {
            let mut state = self.state.lock();
            if context {
                state
                    .contexts
                    .get_mut(&id)
                    .ok_or(DriverError::InvalidInput)?
                    .cleanup = true;
            } else {
                state
                    .resources
                    .get_mut(&id)
                    .ok_or(DriverError::InvalidInput)?
                    .cleanup = true;
            }
        }
        self.cleanup_work
            .as_ref()
            .ok_or(DriverError::Unsupported)?
            .schedule()
    }

    pub(super) fn reclaim(&self) -> DriverResult {
        self.run(&mut |wait| {
            if self.status().is_failed {
                let _ = self.poll();
                return Err(DriverError::BadState);
            }
            // Process bounded snapshots. New enqueue while work runs schedules
            // another pass through the host workqueue's coalescing contract.
            let (contexts, resources) = {
                let state = self.state.lock();
                (
                    state
                        .contexts
                        .iter()
                        .filter_map(|(&id, c)| c.cleanup.then_some(id))
                        .collect::<Vec<_>>(),
                    state
                        .resources
                        .iter()
                        .filter_map(|(&id, r)| r.cleanup.then_some(id))
                        .collect::<Vec<_>>(),
                )
            };
            let mut result = Ok(());
            for id in contexts {
                if let Err(e) = self.destroy_context(id, wait) {
                    result = Err(e);
                }
            }
            for id in resources {
                if let Err(e) = self.destroy_resource(id, wait) {
                    result = Err(e);
                }
            }
            result
        })
    }

    fn present(
        &self,
        id: u32,
        rect: ScanoutRect,
        upload: bool,
        wait: &mut dyn GpuWait,
    ) -> DriverResult {
        if rect.width == 0
            || rect.height == 0
            || rect.x.checked_add(rect.width).is_none()
            || rect.y.checked_add(rect.height).is_none()
        {
            return Err(DriverError::InvalidInput);
        }
        if upload {
            self.request(
                words(
                    0x105,
                    0,
                    &[rect.x, rect.y, rect.width, rect.height, 0, 0, id, 0],
                ),
                wait,
            )?;
        }
        self.request(
            words(
                0x103,
                0,
                &[rect.x, rect.y, rect.width, rect.height, SCANOUT_ID, id],
            ),
            wait,
        )?;
        self.state.lock().active_scanout = id;
        self.request(
            words(0x104, 0, &[rect.x, rect.y, rect.width, rect.height, id, 0]),
            wait,
        )
    }

    fn disable_scanout_locked(&self, wait: &mut dyn GpuWait) -> DriverResult {
        self.request(words(0x103, 0, &[0, 0, 0, 0, SCANOUT_ID, 0]), wait)?;
        // Preserve the previous active ID and all owners if the fence fails.
        self.state.lock().active_scanout = 0;
        Ok(())
    }

    pub(super) fn disable_scanout(&self) -> DriverResult {
        self.run(&mut |wait| self.disable_scanout_locked(wait))
    }

    pub(super) fn present_2d(&self, id: u32, rect: ScanoutRect) -> DriverResult {
        self.run(&mut |wait| {
            self.resource(id)?;
            self.present(id, rect, true, wait)
        })
    }

    pub(super) fn destroy_2d(&self, id: u32) -> DriverResult {
        self.run(&mut |wait| self.destroy_resource(id, wait))
    }

    pub(super) fn create_2d(&self, resource: ScanoutResource, backing: GpuBacking) -> DriverResult {
        let mut backing = Some(backing);
        self.run(&mut |wait| {
            self.create_owned_resource(
                resource.id,
                backing.take().ok_or(DriverError::BadState)?,
                false,
                words(0x101, 0, &[resource.id, 1, resource.width, resource.height]),
                wait,
            )
        })
    }
}

fn words(command: u32, context: u32, payload: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(24 + payload.len() * 4);
    for word in [command, 0, 0, 0, context, 0].iter().chain(payload) {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes
}

impl<H: GpuHal, T: Transport + Send> Drop for State<H, T> {
    fn drop(&mut self) {
        if self.protocol.reset() {
            if let Some(pending) = self.pending.take() {
                // SAFETY: reset ACK stops all DMA. Cancellation unmaps both
                // original streaming mappings exactly once, discarding reply.
                unsafe { pending.transaction.cancel_after_reset() };
            }
            // SAFETY: the transport acknowledged device reset. It can no longer
            // access queue or backing memory; this ManuallyDrop is taken once.
            unsafe { ManuallyDrop::drop(&mut self.protocol) };
        } else {
            // No synchronous reset guarantee: retain bounded DMA allocations,
            // including queue storage. Never turn a failed reset into a UAF.
            if let Some(pending) = self.pending.take() {
                core::mem::forget(pending);
            }
            core::mem::forget(core::mem::take(&mut self.resources));
            log::error!("virtio-gpu: reset unacknowledged; quarantined DMA allocations");
        }
    }
}
