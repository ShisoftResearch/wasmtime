import twasm.Persistent
import twasm.TxnFunc
import twasm.setRoot
import twasm.transaction

@Persistent
class Profile(
    val name: String,
    val tags: MutableList<String>,
    val attrs: MutableMap<String, String>,
)

@TxnFunc
fun installProfile() {
    val tags = mutableListOf("wasm", "pmem")
    val attrs = mutableMapOf("tier" to "research")
    setRoot("profile", Profile("alice", tags, attrs))
}

fun main() {
    transaction {
        installProfile()
    }
}
