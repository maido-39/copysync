package com.copysync.android.sync

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import com.copysync.android.data.Settings

/** Restarts the sync service after boot. A specialUse FGS may be launched from
 *  BOOT_COMPLETED (unlike dataSync, which Android 15+ forbids here). */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == Intent.ACTION_BOOT_COMPLETED && Settings(context).isPaired) {
            SyncService.start(context)
        }
    }
}
