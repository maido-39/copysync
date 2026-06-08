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
        return Notification.Builder(ctx, SERVICE_CHANNEL)
            .setContentTitle(if (connected) "CopySync · connected" else "CopySync")
            .setContentText(text)
            .setSmallIcon(R.drawable.ic_stat_sync)
            .setColor(if (connected) GREEN else GREY)
            .setColorized(true)
            .setOnlyAlertOnce(true)
            .setOngoing(true)
            .setContentIntent(pi)
            .build()
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
