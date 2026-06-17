package twasm

@Target(AnnotationTarget.CLASS)
@Retention(AnnotationRetention.BINARY)
annotation class Persistent

@Target(AnnotationTarget.FUNCTION)
@Retention(AnnotationRetention.BINARY)
annotation class TxnFunc

inline fun transaction(block: () -> Unit) {
    block()
}

@Suppress("UNUSED_PARAMETER")
inline fun <reified T> root(name: String): T {
    error("twasm root marker was not lowered: $name")
}

@Suppress("UNUSED_PARAMETER")
inline fun <reified T> setRoot(name: String, value: T) {
    error("twasm setRoot marker was not lowered: $name")
}
