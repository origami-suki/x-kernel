// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Opt-in per-process syscall observations. Durations cover only calls that
//! begin and complete in one epoch. Outstanding calls remain `started -
//! completed`, never zero-duration samples. CPU follows existing kernel time
//! accounting, including any interrupt time charged there. No hot-path I/O
//! or allocation; this is not a profile of user faults or all kernel work.

use alloc::{string::String, vec, vec::Vec};
use core::{
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering},
};

use kerrno::{KError, KResult};
use klazy::Once;
use kspin::SpinNoPreempt;
use ksync::{Mutex, static_lock};

use crate::{current_cred, current_user_thread};

const SHARDS: usize = 16;
const CAPACITY: usize = 1024;
const MAX_PROBES: usize = 32;
// Release publishes initialized tables; acquire gates readers. Shard locks
// and epoch rechecks prevent late tokens from modifying a new collection.
static ACTIVE_EPOCH: AtomicU64 = AtomicU64::new(0);
static TABLES: Once<Vec<SpinNoPreempt<Table>>> = Once::new();
static_lock! {
    static CONTROL: Mutex<Control> = Mutex::new(Control {
        epoch: 0, start_ns: 0, stop_ns: 0, freeze_end_ns: 0,
    });
}
struct Control {
    epoch: u64,
    start_ns: u64,
    stop_ns: u64,
    freeze_end_ns: u64,
}
#[derive(Clone, Copy, Default)]
struct Entry {
    pid: u32,
    sysno: usize,
    started: u64,
    completed: u64,
    errors: u64,
    cpu_ns: u64,
    elapsed_ns: u64,
    max_cpu_ns: u64,
    max_elapsed_ns: u64,
    cpu_over_elapsed: u64,
}
struct Table {
    epoch: u64,
    entries: Vec<Entry>,
    dropped: u64,
}
impl Table {
    fn new(capacity: usize) -> Self {
        Self {
            epoch: 0,
            entries: vec![Entry::default(); capacity],
            dropped: 0,
        }
    }

    fn reset(&mut self, epoch: u64) {
        self.epoch = epoch;
        self.entries.fill(Entry::default());
        self.dropped = 0;
    }

    fn start(&mut self, pid: u32, sysno: usize) -> Option<usize> {
        let hash = (pid as usize).wrapping_mul(0x9e3779b1) ^ sysno.wrapping_mul(0x85ebca6b);
        for probe in 0..MAX_PROBES.min(self.entries.len()) {
            let slot = hash.wrapping_add(probe) % self.entries.len();
            let entry = &mut self.entries[slot];
            if entry.started == 0 {
                entry.pid = pid;
                entry.sysno = sysno;
            }
            if entry.pid == pid && entry.sysno == sysno {
                entry.started = entry.started.saturating_add(1);
                return Some(slot);
            }
        }
        self.dropped = self.dropped.saturating_add(1);
        None
    }

    fn finish(&mut self, epoch: u64, slot: usize, cpu_ns: u64, elapsed_ns: u64, error: bool) {
        if self.epoch != epoch {
            return;
        }
        let e = &mut self.entries[slot];
        e.completed = e.completed.saturating_add(1);
        e.errors = e.errors.saturating_add(u64::from(error));
        e.cpu_ns = e.cpu_ns.saturating_add(cpu_ns);
        e.elapsed_ns = e.elapsed_ns.saturating_add(elapsed_ns);
        e.max_cpu_ns = e.max_cpu_ns.max(cpu_ns);
        e.max_elapsed_ns = e.max_elapsed_ns.max(elapsed_ns);
        e.cpu_over_elapsed = e
            .cpu_over_elapsed
            .saturating_add(u64::from(cpu_ns > elapsed_ns));
    }
}
/// A syscall interval's scalar identity and clock samples; owns no resources.
/// Only finish once, on the originating thread. Non-returning handlers may
/// abandon the token; their unfinished count remains visible.
pub struct Token {
    epoch: u64,
    shard: usize,
    slot: usize,
    start_ns: u64,
    cpu_ns: u64,
}
fn now_ns() -> u64 {
    khal::time::monotonic_time().as_nanos_u64_saturating()
}
/// Begins observation in syscall task context. No allocation or sleeping lock.
/// Disabled collection performs only an atomic enable check.
pub fn begin(sysno: usize) -> Option<Token> {
    let epoch = ACTIVE_EPOCH.load(Ordering::Acquire);
    if epoch == 0 {
        return None;
    }
    let tables = TABLES.get()?;
    let thread = current_user_thread();
    let shard = thread.tid() as usize % SHARDS;
    let slot = {
        let mut table = tables[shard].lock();
        if table.epoch != epoch || ACTIVE_EPOCH.load(Ordering::Acquire) != epoch {
            return None;
        }
        table.start(thread.pid(), sysno)?
    };
    // Both CPU samples lie inside the wall interval, avoiding systematic
    // CPU > elapsed artifacts from opposite clock-sampling order.
    let start_ns = now_ns();
    let cpu_ns = thread.sample_cpu_time().1.as_nanos_u64_saturating();
    Some(Token {
        epoch,
        shard,
        slot,
        start_ns,
        cpu_ns,
    })
}
/// Completes on the originating syscall thread; ignores stale epochs.
/// `error` is the adapter's error status, not a possibly restored register.
/// Requires task context; uses only short non-sleeping locks.
pub fn finish(token: Option<Token>, error: bool) {
    let Some(t) = token else {
        return;
    };
    if ACTIVE_EPOCH.load(Ordering::Acquire) != t.epoch {
        return;
    }
    let cpu_ns = current_user_thread()
        .sample_cpu_time()
        .1
        .as_nanos_u64_saturating();
    let end_ns = now_ns();
    let Some(tables) = TABLES.get() else {
        return;
    };
    let mut table = tables[t.shard].lock();
    if ACTIVE_EPOCH.load(Ordering::Acquire) == t.epoch {
        table.finish(
            t.epoch,
            t.slot,
            cpu_ns.saturating_sub(t.cpu_ns),
            end_ns.saturating_sub(t.start_ns),
            error,
        );
    }
}
fn require_root() -> KResult<()> {
    if current_cred().euid() != 0 {
        return Err(KError::PermissionDenied);
    }
    Ok(())
}
/// Handles root-only `start` or `stop` in sleepable user-thread context.
/// Start allocates fixed tables once and resets the last stopped result;
/// starting while active returns EBUSY. Stop is idempotent and drains writers.
/// Empty O_TRUNC writes do nothing. The data path never takes CONTROL.
pub fn command(data: &[u8]) -> KResult<()> {
    require_root()?;
    if data.len() > 32 {
        return Err(KError::InvalidInput);
    }
    let text = core::str::from_utf8(data)
        .map_err(|_| KError::InvalidInput)?
        .trim();
    if text.is_empty() {
        return Ok(());
    }
    let mut c = CONTROL.lock();
    match text {
        "start" => {
            if ACTIVE_EPOCH.load(Ordering::Acquire) != 0 {
                return Err(KError::ResourceBusy);
            }
            let epoch = c.epoch.checked_add(1).ok_or(KError::InvalidInput)?;
            let tables = TABLES.call_once(|| {
                (0..SHARDS)
                    .map(|_| SpinNoPreempt::new(Table::new(CAPACITY)))
                    .collect()
            });
            for table in tables {
                table.lock().reset(epoch);
            }
            c.epoch = epoch;
            c.start_ns = now_ns();
            c.stop_ns = 0;
            c.freeze_end_ns = 0;
            ACTIVE_EPOCH.store(epoch, Ordering::Release);
        }
        "stop" => {
            if ACTIVE_EPOCH.load(Ordering::Acquire) != 0 {
                c.stop_ns = now_ns();
                ACTIVE_EPOCH.store(0, Ordering::Release);
                if let Some(tables) = TABLES.get() {
                    for table in tables {
                        table.lock().epoch = 0;
                    }
                }
                c.freeze_end_ns = now_ns();
            }
        }
        _ => return Err(KError::InvalidInput),
    }
    Ok(())
}
/// Exports a consistent stopped snapshot for root; returns EBUSY while active.
/// Allocation/formatting happen outside shard locks. Retains exited PIDs;
/// PID reuse within a window is not disambiguated. Boundary calls are excluded.
pub fn snapshot() -> KResult<String> {
    require_root()?;
    let c = CONTROL.lock();
    if ACTIVE_EPOCH.load(Ordering::Acquire) != 0 {
        return Err(KError::ResourceBusy);
    }
    let mut out = String::new();
    writeln!(
        out,
        "SYSCALL_PROFILE version=1 epoch={} start_ns={} stop_ns={} freeze_end_ns={} shards={} \
         capacity={} max_probes={}",
        c.epoch, c.start_ns, c.stop_ns, c.freeze_end_ns, SHARDS, CAPACITY, MAX_PROBES
    )
    .unwrap();
    out.push_str(
        "shard,pid,sysno,started,completed,errors,cpu_ns,elapsed_ns,max_cpu_ns,max_elapsed_ns,\
         cpu_over_elapsed\n",
    );
    let mut dropped = 0u64;
    if let Some(tables) = TABLES.get() {
        let mut rows = vec![Entry::default(); CAPACITY];
        for (shard, table) in tables.iter().enumerate() {
            {
                let table = table.lock();
                rows.copy_from_slice(&table.entries);
                dropped = dropped.saturating_add(table.dropped);
            }
            for e in &rows {
                if e.started != 0 {
                    writeln!(
                        out,
                        "{shard},{},{},{},{},{},{},{},{},{},{}",
                        e.pid,
                        e.sysno,
                        e.started,
                        e.completed,
                        e.errors,
                        e.cpu_ns,
                        e.elapsed_ns,
                        e.max_cpu_ns,
                        e.max_elapsed_ns,
                        e.cpu_over_elapsed
                    )
                    .unwrap();
                }
            }
        }
    }
    writeln!(out, "SYSCALL_PROFILE_END dropped={dropped}").unwrap();
    Ok(out)
}
#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;
    #[def_test]
    fn profile_keeps_errors_and_unfinished_calls_separate() {
        let mut t = Table::new(4);
        t.reset(1);
        let slot = t.start(7, 63).unwrap();
        assert_eq!(t.start(7, 63), Some(slot));
        t.finish(1, slot, 10, 100, true);
        let e = t.entries[slot];
        assert_eq!((e.started, e.completed, e.errors), (2, 1, 1));
        assert_eq!((e.cpu_ns, e.elapsed_ns), (10, 100));
    }
    #[def_test]
    fn profile_rejects_old_completions_after_reset() {
        let mut t = Table::new(4);
        t.reset(1);
        let old = t.start(7, 63).unwrap();
        t.reset(2);
        t.finish(1, old, 10, 100, false);
        assert_eq!(
            t.entries
                .iter()
                .map(|e| e.completed + e.started)
                .sum::<u64>(),
            0
        );
    }
    #[def_test]
    fn profile_reports_capacity_loss_without_overwriting() {
        let mut t = Table::new(2);
        t.reset(1);
        assert!(t.start(1, 63).is_some());
        assert!(t.start(2, 64).is_some());
        assert!(t.start(3, 65).is_none());
        assert_eq!(t.dropped, 1);
        assert_eq!(t.entries.iter().map(|e| e.started).sum::<u64>(), 2);
    }
    #[def_test]
    fn profile_preserves_maxima_and_clock_anomalies() {
        let mut t = Table::new(4);
        t.reset(1);
        let slot = t.start(7, 63).unwrap();
        t.finish(1, slot, 120, 100, false);
        t.start(7, 63);
        t.finish(1, slot, 20, 300, false);
        let e = t.entries[slot];
        assert_eq!((e.cpu_ns, e.elapsed_ns), (140, 400));
        assert_eq!(
            (e.max_cpu_ns, e.max_elapsed_ns, e.cpu_over_elapsed),
            (120, 300, 1)
        );
    }
}
