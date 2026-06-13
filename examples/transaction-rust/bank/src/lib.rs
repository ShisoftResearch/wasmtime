#![no_std]

use wasmtime_transaction_sdk::{Persist, Root, txn_func};

#[derive(Clone, Copy, Persist)]
#[repr(C)]
pub struct Account {
    pub balance: i64,
}

#[derive(Clone, Copy, Persist)]
#[repr(C)]
pub struct Bank {
    pub alice: Account,
    pub bob: Account,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Request {
    pub from: u32,
    pub to: u32,
    pub amount: i64,
}

const BANK_ROOT: Root<Bank> = Root::tmemory_offset(0);

#[unsafe(no_mangle)]
pub extern "C" fn init_bank(a: i64, b: i64) {
    wasmtime_transaction_sdk::transaction!(tx, {
        let bank = tx.root_mut(&BANK_ROOT);
        let bank = bank.as_mut();
        bank.alice.balance = a;
        bank.bob.balance = b;
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn transfer(from: u32, to: u32, amount: i64) {
    let req = Request { from, to, amount };
    wasmtime_transaction_sdk::transaction!(tx, {
        let bank = tx.root_mut(&BANK_ROOT);
        transfer_impl(bank.as_mut(), &req);
    })
}

#[txn_func]
fn transfer_impl(bank: &mut Bank, req: &Request) {
    if req.from & 1 == 0 {
        bank.alice.balance -= req.amount;
    } else {
        bank.bob.balance -= req.amount;
    }

    if req.to & 1 == 0 {
        bank.alice.balance += req.amount;
    } else {
        bank.bob.balance += req.amount;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn balance(index: u32) -> i64 {
    wasmtime_transaction_sdk::transaction!(tx, {
        let bank = tx.root_ref(&BANK_ROOT);
        let bank = bank.as_ref();
        if index & 1 == 0 {
            bank.alice.balance
        } else {
            bank.bob.balance
        }
    })
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}
