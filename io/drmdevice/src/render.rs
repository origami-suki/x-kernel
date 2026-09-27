// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Render-only descriptions sharing the same physical DRM device.

use super::*;

/// Render-node operations over the same device as its primary card.
///
/// Independent opens receive independent GEM/context state. Render descriptions
/// reject modesetting, authentication and master operations before user access.
pub struct RenderNode {
    card: Arc<Card0>,
}

impl RenderNode {
    /// Creates a render operation table sharing card resource IDs and backing.
    pub fn new(card: Arc<Card0>) -> Arc<Self> {
        Arc::new(Self { card })
    }
}

fn allowed(cmd: u32) -> bool {
    if (cmd >> 8) & 0xff == u32::from(DRM_TYPE) && (0x40..=0x5f).contains(&(cmd & 0xff)) {
        return true;
    }
    matches!(
        cmd,
        DrmVersion::CMD
            | DrmUnique::CMD
            | DrmSetVersion::CMD
            | DrmGetCap::CMD
            | DrmSetClientCap::CMD
            | DrmGemClose::CMD
            | DrmPrimeHandle::CMD
            | DrmPrimeFdToHandle::CMD
    )
}

impl DeviceFileOps for RenderNode {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        file.set_private_data(DrmFile::new());
        Ok(())
    }

    fn release(&self, inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        self.card.release(inode, file)
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.card.read(file, buf, offset)
    }

    fn ioctl(&self, file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        if !allowed(cmd) {
            return Err(VfsError::PermissionDenied);
        }
        self.card.ioctl(file, cmd, arg)
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        self.card.mmap(file, mapper)
    }

    fn flags(&self) -> NodeFlags {
        self.card.flags()
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        self.card.poll(file)
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        self.card.register_poll(file, context, events)
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;
    #[def_test]
    fn render_policy_rejects_kms_and_master_but_allows_render_and_prime() {
        assert_eq!(allowed(DRM_IOCTL_SET_MASTER), false);
        assert_eq!(allowed(DRM_IOCTL_DROP_MASTER), false);
        assert_eq!(allowed(DrmModeAtomic::CMD), false);
        assert_eq!(allowed(DrmModeCreateDumb::CMD), false);
        assert_eq!(allowed(DrmModeFbCmd2::CMD), false);
        assert_eq!(allowed(virtgpu::VirtgpuResourceCreate::CMD), true);
        assert_eq!(allowed(DrmGemClose::CMD), true);
        assert_eq!(allowed(DrmPrimeHandle::CMD), true);
    }
}
