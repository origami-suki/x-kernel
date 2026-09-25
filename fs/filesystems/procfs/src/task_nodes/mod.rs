// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process and thread related procfs nodes.
//!
//! Task directory stat queries report two base links plus the current published
//! thread count. The weak process identity does not keep an exited process alive;
//! an old directory FD reports two links once its threads are gone. Lookup and
//! enumeration retain their existing publication checks. Counts are snapshots,
//! not a guarantee that membership remains unchanged after stat returns.

pub(crate) mod mounts;
pub(crate) mod root;
#[cfg(feature = "tee")]
mod tee;
