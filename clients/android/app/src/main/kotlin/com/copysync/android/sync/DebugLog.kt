package com.copysync.android.sync

import kotlinx.coroutines.flow.MutableStateFlow

/**
 * In-memory event log for the Debug tab. Lines always go to logcat; when
 * [enabled] is on, every event is also buffered for diagnosis (no ADB/READ_LOGS
 * needed). Warnings/errors are buffered even when [enabled] is off, so a failure
 * is in the Debug tab after the fact without having to reproduce it.
 */
object DebugLog {
    val enabled = MutableStateFlow(false)
    val lines = MutableStateFlow<List<String>>(emptyList())

    private val buf = ArrayDeque<String>()
    private val fmt = java.text.SimpleDateFormat("HH:mm:ss.SSS", java.util.Locale.US)

    @Synchronized
    private fun add(level: Int, msg: String) {
        android.util.Log.println(level, "CopySync", msg)
        // Warnings/errors are ALWAYS buffered; verbose INFO only when recording is on.
        if (!enabled.value && level != android.util.Log.WARN) return
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
