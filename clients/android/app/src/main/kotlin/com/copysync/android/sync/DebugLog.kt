package com.copysync.android.sync

import kotlinx.coroutines.flow.MutableStateFlow

/**
 * In-memory event log for the Debug tab. Lines always go to logcat; when
 * [enabled] is on, they are also buffered so the user can read/share every
 * sync + capture event for diagnosis (no ADB/READ_LOGS needed).
 */
object DebugLog {
    val enabled = MutableStateFlow(false)
    val lines = MutableStateFlow<List<String>>(emptyList())

    private val buf = ArrayDeque<String>()
    private val fmt = java.text.SimpleDateFormat("HH:mm:ss.SSS", java.util.Locale.US)

    @Synchronized
    private fun add(level: Int, msg: String) {
        android.util.Log.println(level, "CopySync", msg)
        if (!enabled.value) return
        val lvl = if (level == android.util.Log.WARN) "W" else "I"
        buf.addLast("${fmt.format(java.util.Date())} $lvl $msg")
        while (buf.size > 800) buf.removeFirst()
        lines.value = buf.toList()
    }

    fun i(msg: String) = add(android.util.Log.INFO, msg)
    fun w(msg: String) = add(android.util.Log.WARN, msg)

    @Synchronized
    fun clear() {
        buf.clear()
        lines.value = emptyList()
    }
}
