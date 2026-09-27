# devfs DRM publication security

DRM entries exist only when a real display is registered. renderD128 additionally
requires a healthy implemented command backend; host virgl negotiation alone
cannot create it. Both operation tables share the same physical adapter and
host/KMS namespace, with isolated independent opens. Standard VFS access checks
precede callbacks; RenderNode denies KMS, authentication and master ioctls.

The publisher adds no unsafe code and never interprets user addresses. Ioctl
validation, typed fd import/export, owned mappings, synchronization and deferred
DMA cleanup are enforced by drmdevice and its final independently tested backend.
Boot node numbers/names are mirrored from actual devfs nodes into boot sysfs.
No host GPU PCI identity is fabricated for the guest virtual device. Hotplug,
full device-manager authorization and namespace rebuilding remain outside scope.
