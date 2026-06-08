package com.copysync.android.capture

import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import java.io.File
import java.security.MessageDigest

/** Streams a content URI into the on-demand source cache (clip-src/<sha>), used
 *  by both the clipboard engine and the Share target. */
object SourceCache {
    fun cache(ctx: Context, uri: Uri): Captured.Binary? {
        val cr = ctx.contentResolver
        val mime = cr.getType(uri) ?: "application/octet-stream"
        val name = queryName(ctx, uri)
            ?: ("shared." + mime.substringAfter('/').substringBefore(';').ifEmpty { "bin" })
        val dir = File(ctx.cacheDir, "clip-src").apply { mkdirs() }
        val tmp = File(dir, "tmp-${System.nanoTime()}")
        val (sha, size) = runCatching {
            cr.openInputStream(uri).use { input ->
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
            return null
        }
        if (size == 0L) {
            tmp.delete()
            return null
        }
        val dest = File(dir, sha)
        if (dest.exists()) tmp.delete() else tmp.renameTo(dest)
        return Captured.Binary(dest, mime, name, size, sha)
    }

    private fun queryName(ctx: Context, uri: Uri): String? = runCatching {
        ctx.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst() && !c.isNull(0)) c.getString(0) else null
        }
    }.getOrNull()
}
