// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/dev/dri/` — DRM device directory registration.

use alloc::sync::Arc;

use kvfs::{DirMapping, SimpleDir, SimpleFs};

use crate::{DeviceFile, add_device_entry};

/// Register `/dev/dri/card0` if a display device is available.
pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    if !drmdevice::available() {
        return;
    }
    let mut dri_dir = DirMapping::new();
    let Ok(card) = drmdevice::Card0::registered() else {
        return;
    };
    add_device_entry(
        &mut dri_dir,
        "card0",
        DeviceFile::new_character(fs.clone(), kvfs::DeviceId::new(226, 0), card.clone()),
    );
    if drmdevice::render_available() {
        add_device_entry(
            &mut dri_dir,
            "renderD128",
            DeviceFile::new_character(
                fs.clone(),
                kvfs::DeviceId::new(226, 128),
                drmdevice::RenderNode::new(card),
            ),
        );
    }
    root.add("dri", SimpleDir::new_maker(fs, Arc::new(dri_dir)));
}
