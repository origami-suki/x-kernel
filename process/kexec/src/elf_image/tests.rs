// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};

use kernel_elf_parser::ELFHeadersBuilder;
use kerrno::KError;
use unittest::def_test;

use super::{ElfType, ExecLayoutPlan, ValidatedElfImage, find_dynamic_load_bias, user_load_limit};

const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;

#[derive(Clone, Copy)]
struct TestSegment {
    flags: u32,
    offset: u64,
    virtual_address: u64,
    file_size: u64,
    memory_size: u64,
    alignment: u64,
}

fn current_machine() -> u16 {
    #[cfg(target_arch = "aarch64")]
    return 183;
    #[cfg(target_arch = "x86_64")]
    return 62;
    #[cfg(target_arch = "riscv64")]
    return 243;
    #[cfg(target_arch = "loongarch64")]
    return 258;
    #[allow(unreachable_code)]
    0
}

fn test_elf(elf_type: u16, machine: u16, entry: u64, segments: &[TestSegment]) -> Vec<u8> {
    let program_headers_size = PROGRAM_HEADER_SIZE * segments.len();
    let mut file_size = ELF_HEADER_SIZE + program_headers_size;
    for segment in segments {
        file_size = file_size.max(
            usize::try_from(segment.offset + segment.file_size)
                .expect("test segment must fit in usize"),
        );
    }
    let mut data = vec![0; file_size];
    data[0..4].copy_from_slice(b"\x7fELF");
    data[4] = 2;
    data[5] = 1;
    data[6] = 1;
    put_u16(&mut data, 16, elf_type);
    put_u16(&mut data, 18, machine);
    put_u32(&mut data, 20, 1);
    put_u64(&mut data, 24, entry);
    put_u64(&mut data, 32, ELF_HEADER_SIZE as u64);
    put_u16(&mut data, 52, ELF_HEADER_SIZE as u16);
    put_u16(&mut data, 54, PROGRAM_HEADER_SIZE as u16);
    put_u16(&mut data, 56, segments.len() as u16);

    for (index, segment) in segments.iter().enumerate() {
        let start = ELF_HEADER_SIZE + index * PROGRAM_HEADER_SIZE;
        put_u32(&mut data, start, 1);
        put_u32(&mut data, start + 4, segment.flags);
        put_u64(&mut data, start + 8, segment.offset);
        put_u64(&mut data, start + 16, segment.virtual_address);
        put_u64(&mut data, start + 32, segment.file_size);
        put_u64(&mut data, start + 40, segment.memory_size);
        put_u64(&mut data, start + 48, segment.alignment);
    }
    data
}

fn parse_image(data: &[u8]) -> Result<ValidatedElfImage, KError> {
    let builder = ELFHeadersBuilder::new(data).map_err(|_| KError::InvalidExecutable)?;
    let range = builder.ph_range().map_err(|_| KError::InvalidExecutable)?;
    let headers = builder
        .build(&data[range.start as usize..range.end as usize])
        .map_err(|_| KError::InvalidExecutable)?;
    ValidatedElfImage::new(&headers, data.len() as u64)
}

fn basic_segments() -> [TestSegment; 2] {
    [
        TestSegment {
            flags: 5,
            offset: 0,
            virtual_address: 0,
            file_size: 0x1000,
            memory_size: 0x1000,
            alignment: 0x1000,
        },
        TestSegment {
            flags: 6,
            offset: 0x1000,
            virtual_address: 0x3000,
            file_size: 0x800,
            memory_size: 0x1800,
            alignment: 0x1000,
        },
    ]
}

#[def_test]
fn validates_position_independent_load_layout() {
    let data = test_elf(3, current_machine(), 0x100, &basic_segments());
    let image = parse_image(&data).expect("valid ELF must be accepted");

    assert_eq!(image.elf_type(), ElfType::PositionIndependent);
    assert_eq!(image.load_start(), 0);
    assert_eq!(image.load_end(), 0x5000);
    assert_eq!(image.required_alignment(), 0x1000);
    assert_eq!(image.entry(0x20_0000), Ok(0x20_0100));
    assert_eq!(image.program_header_address(0x20_0000), Ok(0x20_0040));
    assert_eq!(image.load_range(0x20_0000), Ok(0x20_0000..0x20_5000));
}

#[def_test]
fn accepts_nonzero_first_load_address() {
    let segments = [TestSegment {
        flags: 5,
        offset: 0,
        virtual_address: 0x4000,
        file_size: 0x1000,
        memory_size: 0x1800,
        alignment: 0x1000,
    }];
    let data = test_elf(3, current_machine(), 0x4100, &segments);
    let image = parse_image(&data).expect("valid nonzero load address must be accepted");

    assert_eq!(image.load_start(), 0x4000);
    assert_eq!(image.load_end(), 0x6000);
}

#[def_test]
fn layout_plan_preserves_fixed_executable_addresses() {
    let segments = [TestSegment {
        flags: 5,
        offset: 0,
        virtual_address: 0x40_0000,
        file_size: 0x1000,
        memory_size: 0x2000,
        alignment: 0x1000,
    }];
    let data = test_elf(2, current_machine(), 0x40_0100, &segments);
    let image = parse_image(&data).expect("fixed executable must be valid");
    let plan = ExecLayoutPlan::new(&image, None).expect("fixed executable must fit");

    assert_eq!(image.elf_type(), ElfType::Executable);
    assert_eq!(plan.executable_bias(), 0);
    assert_eq!(image.entry(plan.executable_bias()), Ok(0x40_0100));
}

#[def_test]
fn rejects_file_range_larger_than_memory_range() {
    let mut segments = basic_segments();
    segments[1].file_size = 0x1900;
    let data = test_elf(3, current_machine(), 0x100, &segments);
    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn rejects_invalid_segment_alignment() {
    let mut segments = basic_segments();
    segments[1].alignment = 0x1800;
    let data = test_elf(3, current_machine(), 0x100, &segments);
    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn rejects_mismatched_file_and_virtual_page_offsets() {
    let mut segments = basic_segments();
    segments[1].offset = 0x1100;
    let data = test_elf(3, current_machine(), 0x100, &segments);
    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn rejects_wrong_machine() {
    let wrong_machine = if current_machine() == 62 { 183 } else { 62 };
    let data = test_elf(3, wrong_machine, 0x100, &basic_segments());
    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn rejects_entry_outside_executable_segment() {
    let data = test_elf(3, current_machine(), 0x3500, &basic_segments());
    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn rejects_entry_in_segment_page_padding() {
    let segments = [TestSegment {
        flags: 5,
        offset: 0x40,
        virtual_address: 0x1040,
        file_size: 0x1000,
        memory_size: 0x1000,
        alignment: 1,
    }];
    let data = test_elf(3, current_machine(), 0x1000, &segments);
    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn rejects_entry_on_page_replaced_without_execute_permission() {
    let segments = [
        TestSegment {
            flags: 5,
            offset: 0,
            virtual_address: 0,
            file_size: 0x1900,
            memory_size: 0x1900,
            alignment: 0x1000,
        },
        TestSegment {
            flags: 4,
            offset: 0x1900,
            virtual_address: 0x1900,
            file_size: 0x700,
            memory_size: 0x700,
            alignment: 0x1000,
        },
    ];
    let data = test_elf(3, current_machine(), 0x1800, &segments);
    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn rejects_overlapping_load_memory_ranges() {
    let segments = [
        TestSegment {
            flags: 5,
            offset: 0,
            virtual_address: 0,
            file_size: 0x2000,
            memory_size: 0x3000,
            alignment: 0x1000,
        },
        TestSegment {
            flags: 6,
            offset: 0x1000,
            virtual_address: 0x1000,
            file_size: 0x1000,
            memory_size: 0x1000,
            alignment: 0x1000,
        },
    ];
    let data = test_elf(3, current_machine(), 0x100, &segments);
    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn rejects_incorrect_explicit_program_header_address() {
    let mut data = test_elf(3, current_machine(), 0x100, &basic_segments());
    let header = ELF_HEADER_SIZE + PROGRAM_HEADER_SIZE;
    put_u32(&mut data, header, 6);
    put_u64(&mut data, header + 8, ELF_HEADER_SIZE as u64);
    put_u64(&mut data, header + 16, 0x800);
    put_u64(&mut data, header + 32, (PROGRAM_HEADER_SIZE * 2) as u64);
    put_u64(&mut data, header + 40, (PROGRAM_HEADER_SIZE * 2) as u64);

    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn rejects_image_without_load_segments() {
    let data = test_elf(3, current_machine(), 0, &[]);
    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn rejects_unsupported_elf_type() {
    let data = test_elf(1, current_machine(), 0x100, &basic_segments());
    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn rejects_big_endian_image() {
    let mut data = test_elf(3, current_machine(), 0x100, &basic_segments());
    data[5] = 2;
    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn rejects_load_address_overflow() {
    let segments = [TestSegment {
        flags: 5,
        offset: 0,
        virtual_address: u64::MAX - 0xfff,
        file_size: 0x1000,
        memory_size: 0x2000,
        alignment: 0x1000,
    }];
    let data = test_elf(3, current_machine(), u64::MAX - 0x800, &segments);
    assert_eq!(parse_image(&data).unwrap_err(), KError::InvalidExecutable);
}

#[def_test]
fn dynamic_interpreter_skips_large_executable() {
    let interpreter_data = test_elf(3, current_machine(), 0x100, &basic_segments());
    let interpreter = parse_image(&interpreter_data).expect("interpreter must be valid");
    let occupied = alloc::vec![0x1000..0x0900_0000];
    let bias = find_dynamic_load_bias(
        &interpreter,
        0x0400_0000,
        user_load_limit().expect("user load limit must be valid"),
        &occupied,
    )
    .expect("space after executable must be found");

    assert_eq!(bias, 0x0900_0000);
    assert_eq!(interpreter.load_range(bias), Ok(0x0900_0000..0x0900_5000));
}

#[def_test]
fn dynamic_interpreter_falls_back_below_hint() {
    let interpreter_data = test_elf(3, current_machine(), 0x100, &basic_segments());
    let interpreter = parse_image(&interpreter_data).expect("interpreter must be valid");
    let occupied = alloc::vec![0x0400_0000..kaddr_layout::USER_HEAP_BASE];
    let bias = find_dynamic_load_bias(
        &interpreter,
        0x0400_0000,
        user_load_limit().expect("user load limit must be valid"),
        &occupied,
    )
    .expect("space below hint must be found");

    assert_eq!(bias, 0x1000);
}

#[def_test]
fn layout_plan_keeps_interpreter_outside_main_image() {
    let executable_segments = [TestSegment {
        flags: 5,
        offset: 0,
        virtual_address: 0,
        file_size: 0x1000,
        memory_size: 0x0800_0000,
        alignment: 0x1000,
    }];
    let executable_data = test_elf(3, current_machine(), 0x100, &executable_segments);
    let executable = parse_image(&executable_data).expect("executable must be valid");
    let interpreter_data = test_elf(3, current_machine(), 0x100, &basic_segments());
    let interpreter = parse_image(&interpreter_data).expect("interpreter must be valid");

    let plan = ExecLayoutPlan::new(&executable, Some(&interpreter))
        .expect("interpreter must fit after executable");
    assert_eq!(plan.executable_bias(), kaddr_layout::USER_SPACE_BASE);
    assert_eq!(plan.interpreter_bias(), Some(0x0800_1000));
}

#[def_test]
fn layout_plan_aligns_position_independent_bias() {
    let segments = [TestSegment {
        flags: 5,
        offset: 0,
        virtual_address: 0,
        file_size: 0x1000,
        memory_size: 0x2000,
        alignment: 0x20_0000,
    }];
    let data = test_elf(3, current_machine(), 0x100, &segments);
    let image = parse_image(&data).expect("large-alignment executable must be valid");
    let plan = ExecLayoutPlan::new(&image, None).expect("executable must fit");

    assert_eq!(plan.executable_bias(), 0x20_0000);
}

#[def_test]
fn interpreter_search_reports_exhausted_range() {
    let data = test_elf(3, current_machine(), 0x100, &basic_segments());
    let image = parse_image(&data).expect("interpreter must be valid");
    let limit = user_load_limit().expect("user load limit must be valid");
    let occupied = [limit.clone()];

    assert_eq!(
        find_dynamic_load_bias(&image, kaddr_layout::USER_INTERP_BASE, limit, &occupied),
        None
    );
}

#[def_test]
fn overlapping_load_pages_are_owned_by_later_segment() {
    let segments = [
        TestSegment {
            flags: 4,
            offset: 0,
            virtual_address: 0,
            file_size: 0x1900,
            memory_size: 0x1900,
            alignment: 0x1000,
        },
        TestSegment {
            flags: 5,
            offset: 0x1900,
            virtual_address: 0x1900,
            file_size: 0x700,
            memory_size: 0x700,
            alignment: 0x1000,
        },
    ];
    let data = test_elf(3, current_machine(), 0x1a00, &segments);
    let image = parse_image(&data).expect("shared load page must be normalized");

    assert_eq!(image.segments().len(), 2);
    assert_eq!(image.segments()[0].mapping_start, 0);
    assert_eq!(image.segments()[0].mapping_end, 0x1000);
    assert_eq!(image.segments()[1].mapping_start, 0x1000);
    assert_eq!(image.segments()[1].mapping_end, 0x2000);
}

fn put_u16(data: &mut [u8], offset: usize, value: u16) {
    data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(data: &mut [u8], offset: usize, value: u64) {
    data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

// Project regressions retained when replacing the original M1-002 loader.
#[def_test]
fn project_segment_pages_match_original_mapping_formula() {
    use memaddr::{MemoryAddr, VirtAddr};

    for page_offset in [0usize, 1, 0x40, 0x800, 0xdd0, 0xfff] {
        for memory_size in [1usize, 0x1000, 0x1fff, 0x2000, 0x3410, 0xa19f4, 0x5000298] {
            let address = 0x1_0000 + page_offset;
            let segments = [
                basic_segments()[0],
                TestSegment {
                    flags: 6,
                    offset: (0x1000 + page_offset) as u64,
                    virtual_address: address as u64,
                    file_size: 0,
                    memory_size: memory_size as u64,
                    alignment: 0x1000,
                },
            ];
            let data = test_elf(3, current_machine(), 0x100, &segments);
            let image = parse_image(&data).expect("valid page-offset fixture");
            let mapped_start = VirtAddr::from_usize(address).align_down_4k().as_usize();
            let mapped_size = VirtAddr::from_usize(page_offset + memory_size)
                .align_up_4k()
                .as_usize();
            assert_eq!(image.segments()[1].mapping_start, mapped_start);
            assert_eq!(image.segments()[1].mapping_end, mapped_start + mapped_size);
            assert_eq!(image.load_end(), mapped_start + mapped_size);
        }
    }
}

#[def_test]
fn project_interpreter_bias_does_not_alias_one_page_hole() {
    use khal::paging::{MappingFlags, PageSize};
    use memaddr::{VirtAddr, VirtAddrRange};
    use memspace::{VmArea, VmAreaSet, VmBackingInfo, VmBackingKind};

    let base = 0x0400_0000usize;
    let segments = [TestSegment {
        flags: 5,
        offset: 0,
        virtual_address: 0x1000,
        file_size: 0x1000,
        memory_size: 0x1000,
        alignment: 0x1000,
    }];
    let data = test_elf(3, current_machine(), 0x1100, &segments);
    let interpreter = parse_image(&data).expect("valid nonzero first load address");
    let ranges = [0x1000..base, (base + 0x1000)..(base + 0x2000)];
    let mut occupied = VmAreaSet::new();
    let flags = MappingFlags::USER | MappingFlags::READ;
    for range in &ranges {
        occupied
            .try_insert(VmArea::new(
                VirtAddr::from_usize(range.start),
                range.end - range.start,
                flags,
                flags,
                VmBackingInfo::new(VmBackingKind::Linear, PageSize::Size4K),
                0,
                None,
            ))
            .expect("non-overlapping main image");
    }
    // The historical mistake uses the vacant mapping start as a load bias.
    let old_mapping = interpreter.load_range(base).expect("old mapping");
    assert!(occupied.overlaps(VirtAddrRange::new(
        VirtAddr::from_usize(old_mapping.start),
        VirtAddr::from_usize(old_mapping.end),
    )));
    let bias = find_dynamic_load_bias(&interpreter, base, 0x1000..(base + 0x1_0000), &ranges)
        .expect("one-page hole is sufficient for this image");
    let mapped = interpreter.load_range(bias).expect("new mapping");
    assert_eq!(mapped, base..(base + 0x1000));
    assert_eq!(bias, base - 0x1000);
    assert!(!occupied.overlaps(VirtAddrRange::new(
        VirtAddr::from_usize(mapped.start),
        VirtAddr::from_usize(mapped.end),
    )));
}
