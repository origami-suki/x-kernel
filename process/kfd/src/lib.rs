// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-local descriptor tables, stable file snapshots, and Linux metadata values.
//!
//! [`FdTable`] stores [`FileDescriptor`] entries referencing `kvfs::VfsFile`.
//! [`FdSnapshot`] retains an open file across descriptor changes; [`Kstat`] handles
//! metadata conversion. Shared tables need external locking, normally via kresources.
//!
//! # Example
//!
//! ```no_run
//! use kfd::FdTable;
//! let table = FdTable::new_shared();
//! assert_eq!(table.read().count(), 0);
//! let copied = FdTable::clone_shared_from(&table);
//! assert_eq!(copied.read().count(), 0);
//! ```

#![no_std]

extern crate alloc;

mod fd_table;
mod file_descriptor;
mod stat;

pub use self::{
    fd_table::FdTable,
    file_descriptor::{FdSnapshot, FileDescriptor},
    stat::Kstat,
};

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use anon_inodefs::AnonInodeFs;
    use kpoll::IoEvents;
    use ktime_types::SystemTime;
    use kvfs::{DeviceId, FMode, FileOperations, OpenFlags, VfsFile, VfsResult};
    use linux_raw_sys::general::stat;
    use unittest::def_test;

    use crate::{FdTable, Kstat};

    struct SnapshotTestFops;

    impl FileOperations for SnapshotTestFops {
        fn poll(&self, _file: &VfsFile) -> IoEvents {
            IoEvents::IN
        }
    }

    struct FlushCountFops {
        flushes: Arc<AtomicUsize>,
        result: VfsResult<()>,
    }

    impl FileOperations for FlushCountFops {
        fn flush(&self, _file: &VfsFile) -> VfsResult<()> {
            self.flushes.fetch_add(1, Ordering::Relaxed);
            self.result
        }
    }

    fn snapshot_test_file() -> alloc::sync::Arc<VfsFile> {
        AnonInodeFs::global()
            .get_file(
                "[snapshot-test]",
                alloc::sync::Arc::new(SnapshotTestFops),
                alloc::sync::Arc::new(()),
                FMode::READ,
                OpenFlags::NONBLOCK,
                kcred::initial_cred(),
            )
            .expect("snapshot test anon inode file opens")
    }

    fn flush_count_test_file(flushes: Arc<AtomicUsize>, result: VfsResult<()>) -> Arc<VfsFile> {
        AnonInodeFs::global()
            .get_file(
                "[fd-table-drop-test]",
                Arc::new(FlushCountFops { flushes, result }),
                Arc::new(()),
                FMode::READ,
                OpenFlags::empty(),
                kcred::initial_cred(),
            )
            .expect("fd table drop test file opens")
    }

    #[def_test]
    fn test_kstat_default() {
        let kstat = Kstat::default();
        assert_eq!(kstat.dev, 0);
        assert_eq!(kstat.ino, 1);
        assert_eq!(kstat.nlink, 1);
        assert_eq!(kstat.mode, 0);
        assert_eq!(kstat.uid, 1);
        assert_eq!(kstat.gid, 1);
        assert_eq!(kstat.size, 0);
        assert_eq!(kstat.blksize, 4096);
        assert_eq!(kstat.blocks, 0);
        assert_eq!(kstat.atime, SystemTime::UNIX_EPOCH);
        assert_eq!(kstat.mtime, SystemTime::UNIX_EPOCH);
        assert_eq!(kstat.ctime, SystemTime::UNIX_EPOCH);
    }

    #[def_test]
    fn test_kstat_to_stat() {
        let kstat = Kstat {
            dev: 42,
            ino: 100,
            nlink: 3,
            mode: 0o755,
            uid: 1000,
            gid: 1000,
            size: 4096,
            blksize: 512,
            blocks: 8,
            rdev: DeviceId::default(),
            atime: SystemTime::from_unix_parts(1000, 500_000_000).unwrap(),
            mtime: SystemTime::from_unix_seconds(2000),
            ctime: SystemTime::from_unix_parts(3000, 123_456_789).unwrap(),
        };

        let s: stat = kstat.into();
        assert_eq!(s.st_dev, 42);
        assert_eq!(s.st_ino, 100);
        assert_eq!(s.st_nlink, 3);
        assert_eq!(s.st_mode, 0o755);
        assert_eq!(s.st_uid, 1000);
        assert_eq!(s.st_gid, 1000);
        assert_eq!(s.st_size, 4096);
        assert_eq!(s.st_blksize, 512);
        assert_eq!(s.st_blocks, 8);
        assert_eq!(s.st_atime, 1000);
        assert_eq!(s.st_atime_nsec, 500_000_000);
        assert_eq!(s.st_mtime, 2000);
        assert_eq!(s.st_mtime_nsec, 0);
        assert_eq!(s.st_ctime, 3000);
        assert_eq!(s.st_ctime_nsec, 123_456_789);
    }

    #[def_test]
    fn test_fd_snapshot_keeps_file_alive_after_close() {
        let mut table = FdTable::default();
        let fd = table.add_file(16, snapshot_test_file(), true).unwrap();

        let snapshot = table.snapshot(fd).unwrap();
        table.file_close_fd_locked(fd).unwrap().close().unwrap();

        assert_eq!(snapshot.fd(), fd);
        assert!(snapshot.cloexec());
        assert_eq!(snapshot.open_flags(), OpenFlags::NONBLOCK);
        assert_eq!(
            snapshot.path().display_path().unwrap(),
            "anon_inode:[snapshot-test]"
        );
        assert!(snapshot.file().poll().contains(IoEvents::IN));
        assert!(matches!(
            table.snapshot(fd),
            Err(kerrno::KError::BadFileDescriptor)
        ));
    }

    #[def_test]
    fn test_duplicate_from_selects_range_and_shares_file() {
        let mut table = FdTable::default();
        let file = snapshot_test_file();
        let source = table.add_file(128, file.clone(), true).unwrap();
        assert_eq!(table.duplicate_from(source, 64, 128, false), Ok(64));
        assert_eq!(table.duplicate_from(source, 64, 128, true), Ok(65));
        assert!(Arc::ptr_eq(&table.get_file(64).unwrap(), &file));
        assert_eq!(table.cloexec(source), Ok(true));
        assert_eq!(table.cloexec(64), Ok(false));
        assert_eq!(table.cloexec(65), Ok(true));
        table.file_close_fd_locked(64).unwrap().close().unwrap();
        assert_eq!(table.duplicate_from(source, 64, 128, false), Ok(64));
        assert_eq!(table.duplicate_from(source, 0, 128, false), Ok(1));
    }

    #[def_test]
    fn test_duplicate_from_validates_source_then_minimum_without_leaks() {
        use kerrno::KError;
        let mut table = FdTable::default();
        let file = snapshot_test_file();
        let source = table.add_file(128, file.clone(), false).unwrap();
        let refs = Arc::strong_count(&file);
        for _ in 0..3 {
            assert_eq!(
                table.duplicate_from(-1, usize::MAX, 128, false),
                Err(KError::BadFileDescriptor)
            );
            assert_eq!(
                table.duplicate_from(2, 128, 128, false),
                Err(KError::BadFileDescriptor)
            );
            assert_eq!(
                table.duplicate_from(source, usize::MAX, 128, false),
                Err(KError::InvalidInput)
            );
            assert_eq!(
                table.duplicate_from(source, 128, 128, false),
                Err(KError::InvalidInput)
            );
            assert_eq!(
                table.duplicate_from(source, 0, 0, false),
                Err(KError::InvalidInput)
            );
            assert_eq!(
                table.duplicate_from(source, krlimit::FILE_LIMIT, u64::MAX, false),
                Err(KError::TooManyOpenFiles)
            );
            assert_eq!(table.count(), 1);
            assert_eq!(Arc::strong_count(&file), refs);
        }
    }

    #[def_test]
    fn test_duplicate_from_bounds_numbers_instead_of_occupied_count() {
        use kerrno::KError;
        let mut table = FdTable::default();
        let source = table.add_file(128, snapshot_test_file(), false).unwrap();
        for fd in 100..128 {
            assert_eq!(table.duplicate_from(source, fd, 128, false), Ok(fd as i32));
        }
        assert_eq!(
            table.duplicate_from(source, 100, 128, false),
            Err(KError::TooManyOpenFiles)
        );
        assert_eq!(table.duplicate_from(source, 1, 2, false), Ok(1));
        assert_eq!(
            table.duplicate_from(source, 0, 2, false),
            Err(KError::TooManyOpenFiles)
        );
        table.file_close_fd_locked(110).unwrap().close().unwrap();
        assert_eq!(table.duplicate_from(source, 100, 128, true), Ok(110));
        assert_eq!(
            table.duplicate_from(source, 1023, u64::MAX, false),
            Ok(1023)
        );
        assert_eq!(
            table.duplicate_from(source, 1023, u64::MAX, false),
            Err(KError::TooManyOpenFiles)
        );
    }

    #[def_test]
    fn test_fd_table_drop_closes_remaining_descriptors() {
        let flushes = Arc::new(AtomicUsize::new(0));
        let mut table = FdTable::default();
        table
            .add_file(16, flush_count_test_file(flushes.clone(), Ok(())), false)
            .unwrap();

        drop(table);

        assert_eq!(flushes.load(Ordering::Relaxed), 1);
    }

    #[def_test]
    fn test_fd_table_drop_continues_after_flush_error() {
        let failed_flushes = Arc::new(AtomicUsize::new(0));
        let successful_flushes = Arc::new(AtomicUsize::new(0));
        let mut table = FdTable::default();
        table
            .add_file(
                16,
                flush_count_test_file(failed_flushes.clone(), Err(kerrno::KError::Io)),
                false,
            )
            .unwrap();
        table
            .add_file(
                16,
                flush_count_test_file(successful_flushes.clone(), Ok(())),
                false,
            )
            .unwrap();

        drop(table);

        assert_eq!(failed_flushes.load(Ordering::Relaxed), 1);
        assert_eq!(successful_flushes.load(Ordering::Relaxed), 1);
    }
}
