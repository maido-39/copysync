package com.copysync.android.capture

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.util.Log
import com.copysync.android.net.sha256Hex
import java.util.concurrent.Executors

/**
 * The heart of the Android client. Triggers on clipboard changes (via the system
 * listener and the logcat-denial watcher), reads the content — directly when we
 * happen to have focus, otherwise through a transient overlay — applies echo
 * suppression, and hands new items to [onCaptured]. Also writes inbound clips
 * back to the OS clipboard.
 */
class ClipboardCaptureEngine(
    private val context: Context,
    private val onCaptured: (String) -> Unit,
) {
    private val clipboard = context.getSystemService(ClipboardManager::class.java)
    private val overlay = OverlayController(context)
    private val guard = EchoGuard()
    private val worker = Executors.newSingleThreadExecutor()

    private val listener = ClipboardManager.OnPrimaryClipChangedListener { trigger() }
    private val logcat = LogcatWatcher(context.packageName) { trigger() }

    @Volatile var lastStatus: String = "idle"
        private set

    fun start() {
        runCatching { clipboard.addPrimaryClipChangedListener(listener) }
        logcat.start()
        lastStatus = "watching"
    }

    fun stop() {
        runCatching { clipboard.removePrimaryClipChangedListener(listener) }
        logcat.stop()
        worker.shutdownNow()
    }

    fun canOverlay(): Boolean = overlay.canOverlay()

    private fun trigger() {
        if (worker.isShutdown) return
        Log.i("CopySync", "clipboard change trigger")
        worker.execute {
            val text = readText()
            if (text.isNullOrEmpty()) {
                lastStatus = "read blocked (need focus/overlay)"
                Log.i("CopySync", "capture: read blocked/empty")
                return@execute
            }
            val sha = sha256Hex(text)
            if (guard.seenRecently(sha)) {
                lastStatus = "echo-suppressed"
                Log.i("CopySync", "capture: echo-suppressed")
                return@execute
            }
            guard.mark(sha)
            lastStatus = "captured ${text.length} chars"
            Log.i("CopySync", "capture: emitting ${text.length} chars")
            onCaptured(text)
        }
    }

    /** Read current clipboard text: directly if possible, else via overlay focus. */
    fun readText(): String? {
        directText()?.let { return it }
        val clip = overlay.readWithFocus() ?: return null
        return clip.firstText()
    }

    private fun directText(): String? = clipboard.primaryClip?.firstText()

    /** Write an inbound clip to the OS clipboard, suppressing the resulting echo. */
    fun applyInbound(text: String) {
        guard.mark(sha256Hex(text))
        val clip = ClipData.newPlainText("CopySync", text)
        if (!overlay.writeWithFocus(clip)) {
            runCatching { clipboard.setPrimaryClip(clip) }
        }
    }

    private fun ClipData.firstText(): String? =
        takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(context)?.toString()?.ifEmpty { null }
}
