// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Best-effort UART transmission without borrowing the normal, locked backend.

#[cfg(feature = "ns16550-mmio")]
use crate::ns16550_mmio::{Port, SerialRegWidth};

// Bound each byte's polling without relying on clocks, IRQs, or another CPU.
const TX_POLL_LIMIT: usize = 100_000;

/// Immutable addresses captured by `SerialPort` after mapping the device.
/// All variants share the existing UART mapping and never reinitialize it.
pub(super) enum EmergencyTx {
    #[cfg(feature = "pl011")]
    Pl011 { base: usize },
    #[cfg(feature = "ns16550-mmio")]
    Ns16550Mmio {
        base: usize,
        stride: usize,
        reg_width: SerialRegWidth,
    },
    #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
    Ns16550IoPort { base: u16 },
}

impl EmergencyTx {
    pub(super) fn write_data(&self, bytes: &[u8]) {
        for &byte in bytes {
            let raw_bytes = match byte {
                b'\n' => &b"\r\n"[..],
                8 | 0x7f if self.expands_backspace() => &b"\x08 \x08"[..],
                _ => core::slice::from_ref(&byte),
            };
            for &raw_byte in raw_bytes {
                if !self.send_raw(raw_byte) {
                    return;
                }
            }
        }
    }

    fn expands_backspace(&self) -> bool {
        match self {
            #[cfg(feature = "pl011")]
            Self::Pl011 { .. } => false,
            #[cfg(feature = "ns16550-mmio")]
            Self::Ns16550Mmio { .. } => true,
            #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
            Self::Ns16550IoPort { .. } => true,
        }
    }

    fn send_raw(&self, byte: u8) -> bool {
        for _ in 0..TX_POLL_LIMIT {
            if self.try_send_raw(byte) {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    fn try_send_raw(&self, byte: u8) -> bool {
        match self {
            #[cfg(feature = "pl011")]
            Self::Pl011 { base } => {
                const FR_OFFSET: usize = 0x18;
                const FR_TX_FULL: u32 = 1 << 5;

                // SAFETY: SerialPort's constructor requires a valid, aligned
                // PL011 mapping for its lifetime. Only device registers are
                // accessed; no reference to the locked Backend is created.
                let flags = unsafe { ((*base + FR_OFFSET) as *const u32).read_volatile() };
                if flags & FR_TX_FULL != 0 {
                    return false;
                }
                // SAFETY: the same mapping contains the 32-bit data register
                // at offset zero. Concurrent hardware writers may lose bytes,
                // but do not access shared Rust-owned mutable storage.
                unsafe { (*base as *mut u32).write_volatile(u32::from(byte)) };
                true
            }
            #[cfg(feature = "ns16550-mmio")]
            Self::Ns16550Mmio {
                base,
                stride,
                reg_width,
            } => {
                // SAFETY: these immutable values came from the same validated
                // mapping/layout as the normal port. This local handle only
                // polls LSR and writes DATA; it never initializes the device.
                let mut port = unsafe { Port::new(*base, *stride, *reg_width) };
                port.try_send_raw(byte).is_ok()
            }
            #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
            Self::Ns16550IoPort { base } => {
                // SAFETY: SerialPort's constructor requires a valid UART I/O
                // base. This local handle does not borrow the locked backend
                // or reprogram the device; try_send_raw only accesses LSR/TX.
                let mut port = unsafe { uart_16550::SerialPort::new(*base) };
                port.try_send_raw(byte).is_ok()
            }
        }
    }
}
