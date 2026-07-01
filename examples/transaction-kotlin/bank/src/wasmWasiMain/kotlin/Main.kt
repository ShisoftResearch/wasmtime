import twasm.Persistent
import twasm.TxnFunc
import twasm.get
import twasm.make_txn_ref
import twasm.root
import twasm.setRoot
import twasm.transaction

@Persistent
class Account(var balance: Long)

@Persistent
class Bank(
    var alice: Account,
    var bob: Account,
    var note: String
)

@TxnFunc
fun transfer(bank: Bank, fromAlice: Boolean, amount: Long, note: String) {
    if (fromAlice) {
        bank.alice.balance -= amount
        bank.bob.balance += amount
    } else {
        bank.bob.balance -= amount
        bank.alice.balance += amount
    }
    bank.note = note
}

@TxnFunc
fun installFreshBank(alice: Long, bob: Long, note: String) {
    val bank = Bank(Account(alice), Account(bob), note)
    setRoot("bank", bank)
}

fun main() {
    val note = make_txn_ref("this is a note")
    transaction {
        installFreshBank(1_000, 200, note.get())
        val bank = root<Bank>("bank")
        transfer(bank, true, 100, note.get())
    }
}
