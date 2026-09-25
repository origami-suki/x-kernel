// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Preserve the byte heap's original capacity when physical buddy indexing changes.

use std::alloc::{Layout, alloc_zeroed, dealloc};

use alloc_engine::{BaseAllocator, ByteAllocator, SlabByteAllocator};

struct Backing {
    ptr: *mut u8,
    layout: Layout,
}

impl Backing {
    fn new(bytes: usize) -> Self {
        let layout = Layout::from_size_align(bytes, bytes).unwrap();
        // SAFETY: The nonzero power-of-two layout is valid. Backing owns the
        // allocation until every nested allocator has been retired.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Self { ptr, layout }
    }
}

impl Drop for Backing {
    fn drop(&mut self) {
        // SAFETY: The nested heap has been dropped before this unique backing
        // allocation, which retains its original allocation layout.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

#[test]
fn slab_large_regions_keep_full_capacity_and_maximum_block() {
    for bytes in [2 << 20, 4 << 20, 16 << 20] {
        let backing = Backing::new(bytes);
        let mut heap = SlabByteAllocator::new();
        heap.init_region(backing.ptr as usize, bytes);
        assert_eq!(heap.total_bytes(), bytes);
        let layout = Layout::from_size_align(bytes, 4096).unwrap();
        for _ in 0..8 {
            let block = heap.allocate(layout).unwrap();
            assert_eq!(block.as_ptr(), backing.ptr);
            assert_eq!(heap.available_bytes(), 0);
            heap.deallocate(block, layout);
            assert_eq!(heap.available_bytes(), bytes);
        }
    }
}

#[test]
fn slab_growth_regions_recover_medium_blocks_without_capacity_loss() {
    let first = Backing::new(2 << 20);
    let second = Backing::new(4 << 20);
    let mut heap = SlabByteAllocator::new();
    heap.init_region(first.ptr as usize, first.layout.size());
    heap.add_region(second.ptr as usize, second.layout.size())
        .unwrap();
    let layout = Layout::from_size_align(1 << 20, 4096).unwrap();
    for round in 0..16 {
        let mut blocks = Vec::new();
        for _ in 0..6 {
            blocks.push(heap.allocate(layout).unwrap());
        }
        assert!(heap.allocate(layout).is_err());
        if round % 2 == 0 {
            blocks.reverse();
        } else {
            blocks.rotate_left(3);
        }
        for ptr in blocks {
            heap.deallocate(ptr, layout);
        }
        assert_eq!(heap.available_bytes(), 6 << 20);
    }
}
