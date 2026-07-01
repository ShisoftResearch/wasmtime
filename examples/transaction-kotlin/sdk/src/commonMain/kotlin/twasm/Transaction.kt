package twasm

@Target(AnnotationTarget.CLASS)
@Retention(AnnotationRetention.BINARY)
annotation class Persistent

@Target(AnnotationTarget.FUNCTION)
@Retention(AnnotationRetention.BINARY)
annotation class TxnFunc

class TxRef<T> internal constructor(val value: T)

@Target(AnnotationTarget.CLASS)
@Retention(AnnotationRetention.BINARY)
annotation class TxCopyable

fun <T> make_txn_ref(value: T): TxRef<T> =
    TxRef(value)

fun <T> TxRef<T>.get(): T =
    value

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
