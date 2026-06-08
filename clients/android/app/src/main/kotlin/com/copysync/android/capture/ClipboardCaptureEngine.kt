package com.copysync.android.capture

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.net.Uri
import android.os.SystemClock
import android.provider.OpenableColumns
import android.util.Log
import androidx.core.content.FileProvider
import com.copysync.android.net.sha256Hex
import java.io.File
import java.security.MessageDigest
import java.util.concurrent.Executors

private const val DEBOUNCE_MS = 350L
private const val SRC_CACHE_CAP_BYTES = 512L * 1024 * 1024 // hold up to 512 MiB of on-demand source files

/** A captured clipboard item: plain text, or binary content (image or file). */
sealed interface Captured {
    data class Text(val text: String) : Captured
    data class Binary(val file: File, val mime: String, val name: String, val size: Long, val sha: String) : Captured
}

/**
 * Triggers on clipboard changes (system listener + logcat-denial watcher), reads
 * the content — directly when focused, otherwise via a transient overlay. Text is
 * emitted inline; any content URI (image or file) is streamed to a source cache
 * (so large items can be served on demand later) and emitted as Binary.
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

    private val srcDir: File get() = File(context.cacheDir, "clip-src").apply { mkdirs() }

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

    /** The held on-demand source file for a blob id, if we still have it. */
    fun heldFile(blobId: String): File? {
        val f = File(srcDir, blobId.removePrefix("sha256:"))
        return if (f.exists()) f else null
    }

    private fun trigger() {
        if (worker.isShutdown) return
        // A single copy makes the system deny every background listener at once;
        // collapse that burst into one read.
        val now = SystemClock.elapsedRealtime()
        if (now - lastTriggerAt < DEBOUNCE_MS) return
        lastTriggerAt = now
        Log.i("CopySync", "clipboard change trigger")
        worker.execute {
            val captured = readClip()
            if (captured == null) {
                lastStatus = "read blocked/empty"
                Log.i("CopySync", "capture: read blocked/empty")
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

    fun readClip(): Captured? {
        clipboard.primaryClip?.let { clipToCaptured(it) }?.let { return it }
        val clip = overlay.readWithFocus() ?: return null
        return clipToCaptured(clip)
    }

    private fun clipToCaptured(clip: ClipData): Captured? {
        if (clip.itemCount == 0) return null
        val item = clip.getItemAt(0)
        item.uri?.let { uri -> cacheUri(uri, clip)?.let { return it } }
        val text = item.coerceToText(context)?.toString()
        return if (!text.isNullOrEmpty()) Captured.Text(text) else null
    }

    /** Stream a content URI to the source cache, computing its sha256. */
    private fun cacheUri(uri: Uri, clip: ClipData): Captured.Binary? {
        val mime = context.contentResolver.getType(uri)
            ?: clip.description?.takeIf { it.mimeTypeCount > 0 }?.getMimeType(0)
            ?: "application/octet-stream"
        val name = queryName(uri) ?: defaultName(mime)
        val dir = srcDir
        val tmp = File(dir, "tmp-${SystemClock.elapsedRealtimeNanos()}")
        val (sha, size) = runCatching {
            context.contentResolver.openInputStream(uri).use { input ->
                requireNotNull(input) { "no input stream" }
                val md = MessageDigest.getInstance("SHA-256")
                var total = 0L
                tmp.outputStream().use { out ->
                    val buf = ByteArray(64 * 1024)
                    while (true) {
                        val n = input.read(buf)
                        if (n < 0) break
                        out.write(buf, 0, n)
                        md.update(buf, 0, n)
                        total += n
                    }
                }
                Pair(md.digest().joinToString("") { "%02x".format(it) }, total)
            }
        }.getOrElse {
            tmp.delete()
            Log.w("CopySync", "capture: could not read uri: ${it.message}")
            return null
        }
        if (size == 0L) {
            tmp.delete()
            return null
        }
        val dest = File(dir, sha)
        if (dest.exists()) tmp.delete() else tmp.renameTo(dest)
        pruneCache(dir)
        return Captured.Binary(dest, mime, name, size, sha)
    }

    private fun queryName(uri: Uri): String? = runCatching {
        context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst() && !c.isNull(0)) c.getString(0) else null
        }
    }.getOrNull()

    private fun defaultName(mime: String): String =
        "clip." + mime.substringAfter('/').substringBefore(';').ifEmpty { "bin" }

    private fun pruneCache(dir: File) {
        val files = dir.listFiles()?.filter { it.isFile } ?: return
        var total = files.sumOf { it.length() }
        if (total <= SRC_CACHE_CAP_BYTES) return
        for (f in files.sortedBy { it.lastModified() }) {
            if (total <= SRC_CACHE_CAP_BYTES) break
            total -= f.length()
            f.delete()
        }
    }

    /** Write inbound text to the clipboard (echo-suppressed). */
    fun applyInbound(text: String) {
        guard.mark(sha256Hex(text))
        val clip = ClipData.newPlainText("CopySync", text)
        if (!overlay.writeWithFocus(clip)) runCatching { clipboard.setPrimaryClip(clip) }
    }

    /** Write an inbound image to the clipboard via a FileProvider URI. */
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
    is Captured.Binary -> sha
}

private fun Captured.describe(): String = when (this) {
    is Captured.Text -> "${text.length} chars"
    is Captured.Binary -> "$name $size bytes ($mime)"
}
