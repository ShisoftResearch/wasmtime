#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use wasmtime_transaction_sdk::{Persist, Root};

#[derive(Persist)]
#[repr(C)]
pub struct Profile {
    pub name: String,
    pub scores: Vec<i64>,
    pub bonus: Box<i64>,
}

const PROFILE_ROOT: Root<Profile> = Root::tmemory_offset(16 * 1024);

#[unsafe(no_mangle)]
pub extern "C" fn init_profile() {
    wasmtime_transaction_sdk::transaction!(tx, {
        tx.write_root(
            &PROFILE_ROOT,
            Profile {
                name: String::with_capacity(32),
                scores: Vec::with_capacity(8),
                bonus: Box::new(7),
            },
        );

        let profile = tx.root_mut(&PROFILE_ROOT);
        let profile = profile.as_mut();
        profile.name.push_str("alice");
        profile.name.push_str("-init");
        profile.scores.push(10);
        profile.scores.push(20);
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn append_profile(value: i64) {
    wasmtime_transaction_sdk::transaction!(tx, {
        let profile = tx.root_mut(&PROFILE_ROOT);
        let profile = profile.as_mut();
        profile.name.push_str("-tx");
        profile.scores.push(value);
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn name_len() -> u32 {
    wasmtime_transaction_sdk::transaction!(tx, {
        tx.root_ref(&PROFILE_ROOT).as_ref().name.len() as u32
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn name_byte(index: u32) -> u32 {
    wasmtime_transaction_sdk::transaction!(tx, {
        tx.root_ref(&PROFILE_ROOT).as_ref().name.as_bytes()[index as usize] as u32
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn score_len() -> u32 {
    wasmtime_transaction_sdk::transaction!(tx, {
        tx.root_ref(&PROFILE_ROOT).as_ref().scores.len() as u32
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn score(index: u32) -> i64 {
    wasmtime_transaction_sdk::transaction!(tx, {
        tx.root_ref(&PROFILE_ROOT).as_ref().scores[index as usize]
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn bonus() -> i64 {
    wasmtime_transaction_sdk::transaction!(tx, { *tx.root_ref(&PROFILE_ROOT).as_ref().bonus })
}

#[unsafe(no_mangle)]
pub extern "C" fn set_bonus(value: i64) {
    wasmtime_transaction_sdk::transaction!(tx, {
        *tx.root_mut(&PROFILE_ROOT).as_mut().bonus = value;
    })
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}
