import twasm.Persistent
import twasm.TxnFunc
import twasm.root
import twasm.setRoot
import twasm.transaction

@Persistent
class Account(var balance: Long)

@Persistent
class Bank(
    var alice: Account,
    var bob: Account,
)

@TxnFunc
fun transfer(bank: Bank, fromAlice: Boolean, amount: Long) {
    if (fromAlice) {
        bank.alice.balance -= amount
        bank.bob.balance += amount
    } else {
        bank.bob.balance -= amount
        bank.alice.balance += amount
    }
}

@TxnFunc
fun installFreshBank(alice: Long, bob: Long) {
    val bank = Bank(Account(alice), Account(bob))
    setRoot("bank", bank)
}

fun main() {
    transaction {
        installFreshBank(1_000, 200)
        val bank = root<Bank>("bank")
        transfer(bank, true, 100)
    }
}
