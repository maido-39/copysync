package com.copysync.android

import android.Manifest
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.ui.Modifier
import com.copysync.android.data.Settings
import com.copysync.android.net.claimAndStore
import com.copysync.android.sync.SyncService
import com.copysync.android.ui.AppRoot

class MainActivity : ComponentActivity() {
    private val requestNotif =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (Build.VERSION.SDK_INT >= 33) {
            requestNotif.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        handleDeepLink(intent)
        // Ensure the sync service is running whenever a paired app is opened.
        if (Settings(this).isPaired) SyncService.start(this)
        setContent {
            MaterialTheme {
                Surface(modifier = Modifier.fillMaxSize()) { AppRoot() }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleDeepLink(intent)
    }

    /**
     * Intent-driven pairing and a test-copy trigger, useful for scripted/headless
     * runs (and a basis for QR/deep-link pairing later):
     *   am start -n .../.MainActivity -e cs_server URL -e cs_otp CODE -e cs_name NAME [-e cs_pin B64]
     *   am start -n .../.MainActivity -e cs_copy "text to copy"
     */
    private fun handleDeepLink(intent: Intent?) {
        intent ?: return
        Log.i(TAG, "deepLink: hasCopy=${intent.hasExtra("cs_copy")} hasPair=${intent.hasExtra("cs_server")}")
        val server = intent.getStringExtra("cs_server")
        val otp = intent.getStringExtra("cs_otp")
        val name = intent.getStringExtra("cs_name")
        if (server != null && otp != null && name != null) {
            val ctx = applicationContext
            Thread {
                runCatching { claimAndStore(ctx, server, otp, name, intent.getStringExtra("cs_pin").orEmpty()) }
                    .onSuccess { Log.i(TAG, "auto-paired as ${it.deviceId}"); SyncService.start(ctx) }
                    .onFailure { Log.e(TAG, "auto-pair failed: ${it.message}") }
            }.start()
        }
        intent.getStringExtra("cs_copy")?.let { text ->
            getSystemService(ClipboardManager::class.java)
                .setPrimaryClip(ClipData.newPlainText("CopySync", text))
            Log.i(TAG, "test copied (${text.length} chars)")
        }
    }

    private companion object {
        const val TAG = "CopySync"
    }
}
