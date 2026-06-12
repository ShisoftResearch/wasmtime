use wasmtime_transaction_sdk::{Persist, Root, transaction_attr};

#[derive(Persist)]
#[repr(C)]
struct Account {
    balance: i64,
}

#[derive(Persist)]
#[repr(C)]
struct Bank {
    accounts: [Account; 2],
}

#[transaction_attr]
fn apply_credit(bank: &mut Bank, amount: i64) {
    bank.accounts[0].balance += amount;
}

#[test]
fn derive_persist_uses_rust_layout_values() {
    assert_eq!(Account::TYPE_NAME, "Account");
    assert_eq!(Account::SIZE, core::mem::size_of::<Account>());
    assert_eq!(Bank::ALIGN, core::mem::align_of::<Bank>());
}

#[test]
fn root_type_checks_derived_persist_types() {
    let root = Root::<Bank>::tmemory_offset(0);
    assert_eq!(root.id().payload(), 0);
}

#[test]
fn transaction_attr_expands_for_mut_helpers() {
    let mut bank = Bank {
        accounts: [Account { balance: 5 }, Account { balance: 7 }],
    };

    apply_credit(&mut bank, 4);

    assert_eq!(bank.accounts[0].balance, 9);
    assert_eq!(bank.accounts[1].balance, 7);
}
