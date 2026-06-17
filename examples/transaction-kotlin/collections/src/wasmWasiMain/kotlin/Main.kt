import twasm.Persistent
import twasm.TxnFunc
import twasm.root
import twasm.setRoot
import twasm.transaction

@Persistent
class Profile(
    var name: String,
    var tags: MutableList<String>,
    var attrs: MutableMap<String, String>,
)

@TxnFunc
fun installProfile() {
    val tags = mutableListOf("wasm", "pmem")
    val attrs = mutableMapOf("tier" to "research")
    setRoot("profile", Profile("alice", tags, attrs))
}

@TxnFunc
fun mutateProfile() {
    val profile = root<Profile>("profile")
    profile.tags.add("gc")
    profile.attrs["status"] = "active"
    profile.name = "alice-updated"
}

fun main() {
    transaction {
        installProfile()
        mutateProfile()
    }
}
