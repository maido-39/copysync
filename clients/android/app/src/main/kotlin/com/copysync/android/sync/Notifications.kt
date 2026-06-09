package com.copysync.android.sync

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import com.copysync.android.MainActivity
import com.copysync.android.R

object Notifications {
    const val SERVICE_CHANNEL = "copysync_service"
    const val CLIP_CHANNEL = "copysync_clips"
    const val SERVICE_NOTIF_ID = 1
    private var nextClipId = 1000

    fun ensureChannels(ctx: Context) {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(SERVICE_CHANNEL, "Sync service", NotificationManager.IMPORTANCE_LOW),
        )
        nm.createNotificationChannel(
            NotificationChannel(CLIP_CHANNEL, "Incoming clips", NotificationManager.IMPORTANCE_DEFAULT),
        )
    }

    private const val GREEN = 0xFF22C55E.toInt()
    private const val GREY = 0xFF9E9E9E.toInt()

    /** The ongoing status-bar notification. Green + colorized while connected;
     *  its text is refreshed on every clip so the status-bar icon visibly reacts. */
    fun serviceNotification(ctx: Context, text: String, connected: Boolean): Notification {
        val pi = PendingIntent.getActivity(
            ctx, 0, Intent(ctx, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val cur = SyncState.currentPool.value
        val b = Notification.Builder(ctx, SERVICE_CHANNEL)
            .setContentTitle(if (connected) "CopySync · 풀: $cur" else "CopySync")
            .setContentText(text)
            .setSmallIcon(R.drawable.ic_stat_sync)
            .setColor(if (connected) GREEN else GREY)
            .setColorized(true)
            .setOnlyAlertOnce(true)
            .setOngoing(true)
            .setContentIntent(pi)
        // Pull-down quick-switch: one action per other pool (max 2 fit comfortably).
        SyncState.pools.value.filter { it != cur }.take(2).forEach { pool ->
            val pIntent = Intent(ctx, SyncService::class.java)
                .setAction(SyncService.ACTION_SET_POOL)
                .putExtra(SyncService.EXTRA_POOL, pool)
            val pPi = PendingIntent.getService(
                ctx, ("pool_$pool").hashCode(), pIntent,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            b.addAction(Notification.Action.Builder(R.drawable.ic_stat_sync, "→ $pool", pPi).build())
        }
        return b.build()
    }

    fun notifyInfo(ctx: Context, title: String, text: String) {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        val n = Notification.Builder(ctx, CLIP_CHANNEL)
            .setContentTitle(title)
            .setContentText(text)
            .setSmallIcon(android.R.drawable.stat_sys_download_done)
            .setAutoCancel(true)
            .build()
        nm.notify(nextClipId++, n)
    }

    /** A received file: tapping opens the app and triggers the save-location picker. */
    fun notifyDownloadable(ctx: Context, origin: String, blobId: String, name: String, mime: String, encrypted: Boolean) {
        val intent = Intent(ctx, MainActivity::class.java).apply {
            putExtra("cs_dl_blob", blobId)
            putExtra("cs_dl_name", name)
            putExtra("cs_dl_mime", mime)
            putExtra("cs_dl_enc", encrypted)
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP
        }
        val pi = PendingIntent.getActivity(
            ctx, blobId.hashCode(), intent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val nm = ctx.getSystemService(NotificationManager::class.java)
        val n = Notification.Builder(ctx, CLIP_CHANNEL)
            .setContentTitle("File from ${origin.ifEmpty { "another device" }}")
            .setContentText("$name — tap to save")
            .setSmallIcon(android.R.drawable.stat_sys_download)
            .setAutoCancel(true)
            .setContentIntent(pi)
            .build()
        nm.notify(nextClipId++, n)
    }

    fun notifyClip(ctx: Context, origin: String, preview: String) {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        val n = Notification.Builder(ctx, CLIP_CHANNEL)
            .setContentTitle("Clipboard from ${origin.ifEmpty { "another device" }}")
            .setContentText(preview.take(120))
            .setSmallIcon(android.R.drawable.ic_menu_save)
            .setAutoCancel(true)
            .build()
        nm.notify(nextClipId++, n)
    }
}
