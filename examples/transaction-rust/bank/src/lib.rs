#![no_std]

use wasmtime_transaction_sdk::{Persist, Root, transaction_attr};

#[derive(Clone, Copy, Persist)]
#[repr(C)]
pub struct Account {
    pub balance: i64,
}

#[derive(Clone, Copy, Persist)]
#[repr(C)]
pub struct Bank {
    pub accounts: [Account; 2],
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
        bank.accounts[0].balance = a;
        bank.accounts[1].balance = b;
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

#[transaction_attr]
fn transfer_impl(bank: &mut Bank, req: &Request) {
    let from = (req.from & 1) as usize;
    let to = (req.to & 1) as usize;
    bank.accounts[from].balance -= req.amount;
    bank.accounts[to].balance += req.amount;
}

#[unsafe(no_mangle)]
pub extern "C" fn balance(index: u32) -> i64 {
    wasmtime_transaction_sdk::transaction!(tx, {
        let bank = tx.root_ref(&BANK_ROOT);
        let bank = bank.as_ref();
        if index & 1 == 0 {
            bank.accounts[0].balance
        } else {
            bank.accounts[1].balance
        }
    })
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}
