// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot-time discovery derived from the registered display device and PCI config.

use alloc::{format, string::String};

use display::DisplayDevice;
use kdevice::{DeviceIdentity, DeviceLocation, PciIdentity, TransportInfo};
use kvfs::{VfsError, VfsResult};

use super::card0::primary_scanout;

/// Immutable PCI identity read from the selected device's actual configuration.
#[derive(Debug, Clone, Copy)]
pub struct DrmPciInfo {
    /// PCI segment/domain.
    pub segment: u16,
    /// PCI bus.
    pub bus: u8,
    /// Device number.
    pub device: u8,
    /// Function number.
    pub function: u8,
    /// Configuration-space vendor identifier.
    pub vendor_id: u16,
    /// Configuration-space device identifier.
    pub device_id: u16,
    /// Configuration-space subsystem vendor.
    pub subsystem_vendor_id: u16,
    /// Configuration-space subsystem device.
    pub subsystem_device_id: u16,
    /// Hardware revision byte.
    pub revision: u8,
    /// Full 24-bit PCI class/subclass/programming-interface value.
    pub class: u32,
}

/// Facts used by boot namespace setup to publish primary/render nodes together.
#[derive(Debug, Clone)]
pub struct DrmDiscovery {
    /// Canonical physical-parent sysfs path, without the `drm/<node>` suffix.
    pub hardware_path: String,
    /// Actual bound driver name in the local device registry.
    pub driver_name: &'static str,
    /// Bus subsystem name when this is a PCI or platform parent.
    pub bus_subsystem: Option<&'static str>,
    /// PCI facts only when verified against the registered PCI identity.
    pub pci: Option<DrmPciInfo>,
}

#[derive(Clone, Copy)]
pub(crate) enum DriverIdentity {
    Minimal2d,
    VirglLegacy,
}

impl DriverIdentity {
    pub(crate) fn detected() -> Self {
        if primary_scanout().is_ok_and(|device| {
            matches!(
                device.parent().transport(),
                Some(TransportInfo::Virtio { device_type: 16 })
            ) && device.has_virgl_commands()
        }) {
            Self::VirglLegacy
        } else {
            Self::Minimal2d
        }
    }

    pub(crate) fn version(self) -> (i32, i32, i32) {
        match self {
            Self::Minimal2d => (
                super::consts::DRIVER_VERSION_MAJOR,
                super::consts::DRIVER_VERSION_MINOR,
                super::consts::DRIVER_VERSION_PATCHLEVEL,
            ),
            Self::VirglLegacy => (0, 0, 0),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Minimal2d => super::consts::DRIVER_NAME,
            Self::VirglLegacy => "virtio_gpu",
        }
    }

    pub(crate) fn date(self) -> &'static str {
        match self {
            Self::Minimal2d => super::consts::DRIVER_DATE,
            Self::VirglLegacy => "20260927",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Minimal2d => super::consts::DRIVER_DESC,
            Self::VirglLegacy => "X-Kernel legacy virtio GPU DRM adapter",
        }
    }
}

/// Returns whether a healthy implemented backend can serve a render node.
pub fn render_available() -> bool {
    primary_scanout()
        .is_ok_and(|device| device.has_virgl_commands() && !device.virgl_command_status().is_failed)
}

fn parse_pci(
    identity: PciIdentity,
    location: DeviceLocation,
    words: [u32; 4],
) -> VfsResult<DrmPciInfo> {
    let DeviceLocation::Pci {
        segment,
        bus,
        device,
        function,
    } = location
    else {
        return Err(VfsError::InvalidInput);
    };
    let vendor_id = words[0] as u16;
    let device_id = (words[0] >> 16) as u16;
    let class = words[1] >> 8;
    if vendor_id != identity.vendor_id
        || device_id != identity.device_id
        || (class >> 16) as u8 != identity.class
        || (class >> 8) as u8 != identity.subclass
        || (words[2] >> 16) & 0x7f != 0
    {
        return Err(VfsError::NoSuchDevice);
    }
    Ok(DrmPciInfo {
        segment,
        bus,
        device,
        function,
        vendor_id,
        device_id,
        subsystem_vendor_id: words[3] as u16,
        subsystem_device_id: (words[3] >> 16) as u16,
        revision: words[1] as u8,
        class,
    })
}

/// Returns an actual display-parent snapshot for sysfs publication.
///
/// # Errors
/// Returns ENODEV when no display exists or configuration no longer matches
/// its registered identity. PCI snapshots use the project's segment-zero ECAM
/// configuration; an unsupported segment or inaccessible mapping fails closed.
/// This boot/task-context query may allocate and map PCI configuration space.
pub fn discovery() -> VfsResult<DrmDiscovery> {
    let display = primary_scanout()?;
    let parent = display.parent();
    let driver_name = display.driver_name();
    match (parent.identity(), display.location()) {
        (
            DeviceIdentity::Pci(identity),
            location @ DeviceLocation::Pci {
                segment,
                bus,
                device,
                function,
            },
        ) => {
            if segment != 0 || device >= 32 || function >= 8 {
                return Err(VfsError::OperationNotSupported);
            }
            // This is the same safe mapping provider/CAM selected by this
            // project's AArch64 PCI backend. No BAR or command register is written.
            let mut view = pci::PciBus::new(pci::Cam::Ecam).map_err(|_| VfsError::Io)?;
            if bus > view.bus_end() {
                return Err(VfsError::NoSuchDevice);
            }
            let (_, config) = view.parts_mut();
            let bdf = pci::DeviceFunction {
                bus,
                device,
                function,
            };
            let info = parse_pci(
                identity,
                location,
                [
                    config.read_word(bdf, 0),
                    config.read_word(bdf, 8),
                    config.read_word(bdf, 12),
                    config.read_word(bdf, 0x2c),
                ],
            )?;
            Ok(DrmDiscovery {
                hardware_path: format!(
                    "/sys/devices/pci{segment:04x}:00/{segment:04x}:{bus:02x}:{device:02x}.\
                     {function}"
                ),
                driver_name,
                bus_subsystem: Some("pci"),
                pci: Some(info),
            })
        }
        (_, DeviceLocation::Mmio { base, .. }) => Ok(DrmDiscovery {
            hardware_path: format!("/sys/devices/platform/virtio-mmio-{base:x}"),
            driver_name,
            bus_subsystem: Some("platform"),
            pci: None,
        }),
        _ => Ok(DrmDiscovery {
            hardware_path: format!(
                "/sys/devices/virtual/xkernel-display-{}",
                display.id().raw()
            ),
            driver_name,
            bus_subsystem: None,
            pci: None,
        }),
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;
    #[def_test]
    fn pci_snapshot_preserves_register_facts_and_rejects_mismatched_identity() {
        let identity = PciIdentity {
            vendor_id: 0x1af4,
            device_id: 0x1050,
            class: 3,
            subclass: 0,
        };
        let location = DeviceLocation::Pci {
            segment: 0,
            bus: 0,
            device: 3,
            function: 0,
        };
        let words = [0x10501af4, 0x03000007, 0, 0x11001af4];
        let info = parse_pci(identity, location, words).unwrap();
        assert_eq!(info.revision, 7);
        assert_eq!(info.subsystem_vendor_id, 0x1af4);
        assert_eq!(info.subsystem_device_id, 0x1100);
        assert_eq!(info.class, 0x030000);
        let mut bad = words;
        bad[0] = 0xffffffff;
        assert_eq!(
            parse_pci(identity, location, bad).err(),
            Some(VfsError::NoSuchDevice)
        );
    }
    #[def_test]
    fn legacy_adapter_version_does_not_claim_explicit_fence_fd_support() {
        assert_eq!(DriverIdentity::VirglLegacy.version(), (0, 0, 0));
        assert_eq!(DriverIdentity::VirglLegacy.name(), "virtio_gpu");
        assert_eq!(DriverIdentity::Minimal2d.version(), (1, 0, 0));
    }
}
