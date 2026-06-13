#![no_std]

use wasmtime_transaction_sdk::{Persist, Root, transaction_attr};

#[derive(Clone, Copy, Persist)]
#[repr(C)]
pub struct Cell {
    pub value: i64,
}

#[derive(Clone, Copy, Persist)]
#[repr(C)]
pub struct Region {
    pub cells: [Cell; 64],
    pub i32_slot: i32,
    pub i64_slot: i64,
    pub f32_slot: f32,
    pub f64_slot: f64,
}

const REGION_ROOT: Root<Region> = Root::tmemory_offset(4096);

#[transaction_attr]
#[unsafe(no_mangle)]
pub extern "C" fn pressure_round(iterations: u32, stride: u32) -> i64 {
    wasmtime_transaction_sdk::transaction!(tx, {
        let region = tx.root_mut(&REGION_ROOT);
        let region = region.as_mut();
        let mut checksum = 0i64;
        let mut i = 0;
        while i < iterations {
            let index = ((i * stride) & 63) as usize;
            region.cells[index].value += i as i64;
            checksum += region.cells[index].value;
            i += 1;
        }
        region.i32_slot += iterations as i32;
        region.i64_slot += checksum;
        region.f32_slot += iterations as f32;
        region.f64_slot += checksum as f64;
        checksum += region.i32_slot as i64;
        checksum += region.i64_slot;
        checksum += region.f32_slot as i64;
        checksum += region.f64_slot as i64;
        checksum
    })
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}
