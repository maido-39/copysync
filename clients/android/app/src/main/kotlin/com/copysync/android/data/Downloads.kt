package com.copysync.android.data

import android.content.ContentValues
import android.content.Context
import android.net.Uri
import android.os.Environment
import android.provider.MediaStore
import com.copysync.android.net.E2eCrypto
import com.copysync.android.net.pinnedClient
import okhttp3.Request
import java.time.Duration

object Downloads {
    /** Downloads a blob (broker-pulls if on demand) into a caller-chosen URI
     *  (e.g. a Storage Access Framework document). Blocking; call off the main thread. */
    fun fetchToUri(ctx: Context, blobId: String, destUri: Uri, encrypted: Boolean) {
        val settings = Settings(ctx)
        val secrets = Secrets(ctx)
        val token = secrets.token ?: error("not paired")
        val pin = settings.spkiPin ?: error("not paired")
        val base = (settings.serverUrl ?: error("not paired")).trimEnd('/')
        val client = pinnedClient(pin, Duration.ofSeconds(120))
        val req = Request.Builder()
            .url("$base/blob/$blobId")
            .header("Authorization", "Bearer $token")
            .get()
            .build()
        client.newCall(req).execute().use { resp ->
            require(resp.isSuccessful) { "download failed (HTTP ${resp.code})" }
            ctx.contentResolver.openOutputStream(destUri).use { out ->
                requireNotNull(out) { "no output stream" }
                if (encrypted) {
                    val pass = secrets.e2ePass
                    val sid = settings.serverId
                    require(!pass.isNullOrEmpty() && sid != null) { "encrypted but no passphrase set" }
                    val key = E2eCrypto.deriveKey(pass, sid)
                    val raw = resp.body?.bytes() ?: ByteArray(0)
                    out.write(E2eCrypto.open(key, raw)) // decrypt in memory (download is user-initiated)
                } else {
                    resp.body?.byteStream()?.copyTo(out)
                }
            }
        }
    }

    /**
     * Downloads a blob — triggering an on-demand pull from the source if needed —
     * and saves it under the public Downloads/CopySync folder via MediaStore.
     * Returns the saved file name. Blocking; call off the main thread.
     */
    fun fetchToDownloads(ctx: Context, blobId: String, name: String, mime: String): String {
        val settings = Settings(ctx)
        val token = Secrets(ctx).token ?: error("not paired")
        val pin = settings.spkiPin ?: error("not paired")
        val base = (settings.serverUrl ?: error("not paired")).trimEnd('/')

        val client = pinnedClient(pin, Duration.ofSeconds(120))
        val req = Request.Builder()
            .url("$base/blob/$blobId")
            .header("Authorization", "Bearer $token")
            .get()
            .build()
        client.newCall(req).execute().use { resp ->
            require(resp.isSuccessful) { "download failed (HTTP ${resp.code})" }
            val values = ContentValues().apply {
                put(MediaStore.Downloads.DISPLAY_NAME, name)
                if (mime.isNotEmpty()) put(MediaStore.Downloads.MIME_TYPE, mime)
                put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/CopySync")
            }
            val uri = ctx.contentResolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                ?: error("could not create download entry")
            ctx.contentResolver.openOutputStream(uri).use { out ->
                requireNotNull(out) { "no output stream" }
                resp.body?.byteStream()?.copyTo(out)
            }
            return name
        }
    }
}
