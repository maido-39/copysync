package com.copysync.android.sync

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import java.io.File

/**
 * In-memory event log for the Debug tab. Lines always go to logcat; when
 * [enabled] is on, every event is also buffered for diagnosis (no ADB/READ_LOGS
 * needed). Warnings/errors are buffered even when [enabled] is off, so a failure
 * is in the Debug tab after the fact without having to reproduce it.
 *
 * When [verbose] ("상세 디버깅") is on, EVERY relevant event is recorded at full
 * fidelity via [v]/[vt] (with ms timestamp + thread + tag) and — critically —
 * mirrored to a rotating file under filesDir/logs/ so the trace survives a crash
 * or process kill. Errors always carry a stack trace via [e].
 */
object DebugLog {
    /** "이벤트 기록": buffers INFO/verbose to the in-memory view. */
    val enabled = MutableStateFlow(false)

    /** "상세 디버깅": records EVERY event (ms+thread+tag) and persists to file. */
    val verbose = MutableStateFlow(false)

    val lines = MutableStateFlow<List<String>>(emptyList())

    private val buf = ArrayDeque<String>()
    private val fmt = java.text.SimpleDateFormat("HH:mm:ss.SSS", java.util.Locale.US)
    private val fileFmt = java.text.SimpleDateFormat("yyyy-MM-dd HH:mm:ss.SSS", java.util.Locale.US)

    // ---- rotating file sink (set once via init) ----
    @Volatile private var logDir: File? = null
    private const val MAX_FILE_BYTES = 512L * 1024 // 512 KB per file
    private const val MAX_FILES = 4 // copysync.log + .1 .. .3

    /**
     * Wire up the persistent file sink. Safe to call repeatedly. Loads the
     * persisted verbose preference so detailed logging is on from process start
     * (e.g. after a crash-restart) without the user re-toggling it.
     */
    fun init(context: Context) {
        runCatching {
            val dir = File(context.filesDir, "logs").apply { mkdirs() }
            logDir = dir
            verbose.value = context
                .getSharedPreferences("copysync", Context.MODE_PRIVATE)
                .getBoolean("verboseDebug", false)
        }
    }

    @Synchronized
    private fun add(level: Int, msg: String, toFile: Boolean) {
        android.util.Log.println(level, "CopySync", msg)
        if (toFile || verbose.value) appendFile(level, msg)
        // Warnings/errors are ALWAYS buffered; INFO when recording or verbose is on.
        if (!enabled.value && !verbose.value && level != android.util.Log.WARN) return
        val lvl = when (level) {
            android.util.Log.WARN -> "W"
            android.util.Log.ERROR -> "E"
            else -> "I"
        }
        buf.addLast("${fmt.format(java.util.Date())} $lvl $msg")
        while (buf.size > 1200) buf.removeFirst()
        lines.value = buf.toList()
    }

    /** Compose the rich verbose prefix: ms timestamp + thread + tag. */
    private fun rich(tag: String, msg: String): String {
        val t = Thread.currentThread().name
        return "[$tag][$t] $msg"
    }

    fun i(msg: String) = add(android.util.Log.INFO, msg, toFile = false)
    fun w(msg: String) = add(android.util.Log.WARN, msg, toFile = false)

    /**
     * An error: ALWAYS logs the full stack trace, and ALWAYS persists to file
     * regardless of the verbose toggle (a crash trail must survive).
     */
    fun e(tag: String, msg: String, t: Throwable? = null) {
        val full = if (t != null) "${rich(tag, msg)}\n${t.stackTraceToString()}" else rich(tag, msg)
        add(android.util.Log.ERROR, full, toFile = true)
    }

    /** A verbose event (tagged). Recorded + persisted ONLY when verbose is on. */
    fun v(tag: String, msg: String) {
        if (!verbose.value) return
        add(android.util.Log.INFO, rich(tag, msg), toFile = true)
    }

    /** Lazy verbose event — the message lambda is skipped entirely when off. */
    inline fun v(tag: String, msg: () -> String) {
        if (!verbose.value) return
        v(tag, msg())
    }

    @Synchronized
    private fun appendFile(level: Int, msg: String) {
        val dir = logDir ?: return
        runCatching {
            val main = File(dir, "copysync.log")
            if (main.exists() && main.length() > MAX_FILE_BYTES) rotate(dir, main)
            val lvl = when (level) {
                android.util.Log.WARN -> "W"
                android.util.Log.ERROR -> "E"
                else -> "I"
            }
            main.appendText("${fileFmt.format(java.util.Date())} $lvl $msg\n")
        }
    }

    private fun rotate(dir: File, main: File) {
        // copysync.log.(N-1) -> .N, drop the oldest, then main -> .1
        for (i in MAX_FILES - 1 downTo 1) {
            val src = File(dir, "copysync.log.$i")
            if (src.exists()) {
                if (i == MAX_FILES - 1) src.delete()
                else src.renameTo(File(dir, "copysync.log.${i + 1}"))
            }
        }
        main.renameTo(File(dir, "copysync.log.1"))
    }

    /** Read the persisted log files (oldest→newest) for export/sharing. */
    @Synchronized
    fun fileContents(): String {
        val dir = logDir ?: return ""
        val sb = StringBuilder()
        for (i in MAX_FILES - 1 downTo 1) {
            val f = File(dir, "copysync.log.$i")
            if (f.exists()) runCatching { sb.append(f.readText()) }
        }
        val main = File(dir, "copysync.log")
        if (main.exists()) runCatching { sb.append(main.readText()) }
        return sb.toString()
    }

    @Synchronized
    fun clear() {
        buf.clear()
        lines.value = emptyList()
        logDir?.let { dir ->
            runCatching {
                File(dir, "copysync.log").delete()
                for (i in 1 until MAX_FILES) File(dir, "copysync.log.$i").delete()
            }
        }
    }
}
