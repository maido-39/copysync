package com.copysync.android.sync

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import com.copysync.android.MainActivity

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

    fun serviceNotification(ctx: Context, text: String): Notification {
        val pi = PendingIntent.getActivity(
            ctx, 0, Intent(ctx, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        return Notification.Builder(ctx, SERVICE_CHANNEL)
            .setContentTitle("CopySync")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.ic_menu_save)
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
