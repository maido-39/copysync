package com.copysync.android

import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import com.copysync.android.capture.SourceCache
import com.copysync.android.data.Settings
import com.copysync.android.sync.SyncService
import kotlin.concurrent.thread

/**
 * Receives files shared from other apps (Share sheet → CopySync) and sends them
 * through the sync — the workaround for arbitrary/large files that can't ride the
 * clipboard. Caches each shared URI to the source cache (while it still holds the
 * share's read grant), then hands it to the service to send (eager or on demand).
 */
class ShareActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (!Settings(this).isPaired) {
            Toast.makeText(this, "CopySync: pair the device first", Toast.LENGTH_LONG).show()
            finish()
            return
        }
        val uris = extractUris(intent)
        if (uris.isEmpty()) {
            finish()
            return
        }
        val ctx = applicationContext
        Toast.makeText(this, "CopySync: sending ${uris.size} item(s)…", Toast.LENGTH_SHORT).show()
        thread {
            for (uri in uris) {
                val b = SourceCache.cache(ctx, uri) ?: continue
                SyncService.shareFile(ctx, b.sha, b.name, b.mime, b.size)
            }
            runOnUiThread { finish() }
        }
    }

    private fun extractUris(intent: Intent): List<Uri> = when (intent.action) {
        Intent.ACTION_SEND -> listOfNotNull(streamExtra(intent))
        Intent.ACTION_SEND_MULTIPLE -> streamListExtra(intent)
        else -> emptyList()
    }

    @Suppress("DEPRECATION")
    private fun streamExtra(intent: Intent): Uri? =
        if (Build.VERSION.SDK_INT >= 33) intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
        else intent.getParcelableExtra(Intent.EXTRA_STREAM)

    @Suppress("DEPRECATION")
    private fun streamListExtra(intent: Intent): List<Uri> =
        (if (Build.VERSION.SDK_INT >= 33) intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
        else intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM)) ?: emptyList()
}
