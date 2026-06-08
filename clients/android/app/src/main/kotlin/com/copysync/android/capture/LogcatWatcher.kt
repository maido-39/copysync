package com.copysync.android.capture

import android.util.Log
import kotlin.concurrent.thread

/**
 * Watches the app's own logcat for the system's "Denying clipboard access to
 * <pkg>, application is not in focus" line, which Android emits when a background
 * clipboard read is blocked. That line is the reliable cross-version signal that
 * the clipboard changed while we lacked focus. Requires the READ_LOGS permission
 * (granted via ADB or Shizuku).
 */
class LogcatWatcher(
    private val pkg: String,
    private val onDenied: () -> Unit,
) {
    @Volatile private var running = false
    private var worker: Thread? = null
    private var process: Process? = null

    fun start() {
        if (running) return
        running = true
        worker = thread(isDaemon = true, name = "copysync-logcat") {
            try {
                // -T 1 starts near "now" so we don't replay an old backlog of denials.
                val proc = ProcessBuilder("logcat", "-T", "1", "-v", "brief")
                    .redirectErrorStream(true)
                    .start()
                process = proc
                proc.inputStream.bufferedReader().useLines { lines ->
                    for (line in lines) {
                        if (!running) break
                        if (line.contains("Denying clipboard access") && line.contains(pkg)) {
                            onDenied()
                        }
                    }
                }
            } catch (e: Exception) {
                Log.w("CopySync", "logcat watcher stopped: ${e.message}")
            }
        }
    }

    fun stop() {
        running = false
        runCatching { process?.destroy() }
        process = null
        worker?.interrupt()
        worker = null
    }
}
