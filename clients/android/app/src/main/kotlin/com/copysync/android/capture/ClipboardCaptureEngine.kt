package com.copysync.android.capture

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.SystemClock
import android.util.Log
import androidx.core.content.FileProvider
import com.copysync.android.net.sha256Hex
import java.io.File
import java.util.concurrent.Executors

private const val DEBOUNCE_MS = 350L

/** A captured clipboard item: plain text, or an image (raw bytes + mime). */
sealed interface Captured {
    data class Text(val text: String) : Captured
    data class Image(val bytes: ByteArray, val mime: String, val name: String) : Captured
}

/**
 * Triggers on clipboard changes (system listener + logcat-denial watcher), reads
 * the content — directly when focused, otherwise via a transient overlay — and
 * hands new text/image items to [onCaptured]. Also writes inbound items back.
 */
class ClipboardCaptureEngine(
    private val context: Context,
    private val onCaptured: (Captured) -> Unit,
) {
    private val clipboard = context.getSystemService(ClipboardManager::class.java)
    private val overlay = OverlayController(context)
    private val guard = EchoGuard()
    private val worker = Executors.newSingleThreadExecutor()

    private val listener = ClipboardManager.OnPrimaryClipChangedListener { trigger() }
    private val logcat = LogcatWatcher(context.packageName) { trigger() }

    @Volatile var lastStatus: String = "idle"
        private set

    @Volatile private var lastTriggerAt = 0L

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
        // A single copy makes the system deny every background listener at once,
        // producing a burst of denial lines; collapse them into one read.
        val now = SystemClock.elapsedRealtime()
        if (now - lastTriggerAt < DEBOUNCE_MS) return
        lastTriggerAt = now
        Log.i("CopySync", "clipboard change trigger")
        worker.execute {
            val captured = readClip()
            if (captured == null) {
                lastStatus = "read blocked/empty"
                Log.i("CopySync", "capture: read blocked/empty (non-text/non-image or denied)")
                return@execute
            }
            val key = captured.dedupKey()
            if (guard.seenRecently(key)) {
                lastStatus = "echo-suppressed"
                Log.i("CopySync", "capture: echo-suppressed")
                return@execute
            }
            guard.mark(key)
            lastStatus = "captured"
            Log.i("CopySync", "capture: emitting ${captured.describe()}")
            onCaptured(captured)
        }
    }

    /** Read the current clip: directly if focused, else via transient overlay focus. */
    fun readClip(): Captured? {
        clipboard.primaryClip?.let { clipToCaptured(it) }?.let { return it }
        val clip = overlay.readWithFocus() ?: return null
        return clipToCaptured(clip)
    }

    private fun clipToCaptured(clip: ClipData): Captured? {
        if (clip.itemCount == 0) return null
        val item = clip.getItemAt(0)
        val uri = item.uri
        if (uri != null) {
            val type = context.contentResolver.getType(uri)
                ?: clip.description?.takeIf { it.mimeTypeCount > 0 }?.getMimeType(0)
            if (type != null && type.startsWith("image/")) {
                val bytes = runCatching {
                    context.contentResolver.openInputStream(uri)?.use { it.readBytes() }
                }.getOrNull()
                if (bytes != null && bytes.isNotEmpty()) {
                    val ext = type.substringAfter('/').substringBefore(';').ifEmpty { "img" }
                    return Captured.Image(bytes, type, "clip-${sha256Hex(bytes).take(12)}.$ext")
                }
            }
        }
        val text = item.coerceToText(context)?.toString()
        return if (!text.isNullOrEmpty()) Captured.Text(text) else null
    }

    /** Write inbound text to the clipboard (echo-suppressed). */
    fun applyInbound(text: String) {
        guard.mark(sha256Hex(text))
        val clip = ClipData.newPlainText("CopySync", text)
        if (!overlay.writeWithFocus(clip)) runCatching { clipboard.setPrimaryClip(clip) }
    }

    /** Write an inbound image to the clipboard via a FileProvider URI (experimental). */
    fun applyInboundImage(bytes: ByteArray, name: String) {
        guard.mark(sha256Hex(bytes))
        val clip = imageClip(bytes, name) ?: return
        if (!overlay.writeWithFocus(clip)) runCatching { clipboard.setPrimaryClip(clip) }
    }

    private fun imageClip(bytes: ByteArray, name: String): ClipData? = runCatching {
        val dir = File(context.cacheDir, "clip-out").apply { mkdirs() }
        val f = File(dir, name).apply { writeBytes(bytes) }
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", f)
        ClipData.newUri(context.contentResolver, "CopySync", uri)
    }.getOrNull()
}

private fun Captured.dedupKey(): String = when (this) {
    is Captured.Text -> sha256Hex(text)
    is Captured.Image -> sha256Hex(bytes)
}

private fun Captured.describe(): String = when (this) {
    is Captured.Text -> "${text.length} chars"
    is Captured.Image -> "image ${bytes.size} bytes ($mime)"
}
