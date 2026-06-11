package com.copysync.android.sync

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import com.copysync.android.MainActivity
import com.copysync.android.R

object Notifications {
    const val SERVICE_CHANNEL = "copysync_service"
    const val CLIP_CHANNEL = "copysync_clips"
    const val POPUP_CHANNEL = "copysync_clip_popup" // HIGH importance: heads-up + transient
    const val SERVICE_NOTIF_ID = 1
    const val INCOMING_ID = 2000 // single, self-replacing clip popup (does not stack)
    private var nextDlId = 3000

    fun ensureChannels(ctx: Context) {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(SERVICE_CHANNEL, "Sync service", NotificationManager.IMPORTANCE_LOW),
        )
        nm.createNotificationChannel(
            NotificationChannel(CLIP_CHANNEL, "Incoming files", NotificationManager.IMPORTANCE_DEFAULT),
        )
        nm.createNotificationChannel(
            NotificationChannel(POPUP_CHANNEL, "Incoming clips (popup)", NotificationManager.IMPORTANCE_HIGH).apply {
                description = "다른 기기에서 복사된 항목을 미리보기와 함께 잠깐 띄웁니다."
            },
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
        nm.notify(nextDlId++, n)
    }

    /** Friendly name for an origin device id, resolved from the live roster. */
    fun deviceName(origin: String): String =
        SyncState.roster.value.firstOrNull { it.id == origin }?.name?.takeIf { it.isNotEmpty() }
            ?: "다른 기기"

    private fun openAppPi(ctx: Context): PendingIntent =
        PendingIntent.getActivity(ctx, 0, Intent(ctx, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)

    /** Decode a downsampled bitmap for a notification preview (null on failure). */
    private fun thumb(data: ByteArray?, max: Int = 512): Bitmap? {
        if (data == null || data.isEmpty()) return null
        return runCatching {
            val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
            BitmapFactory.decodeByteArray(data, 0, data.size, bounds)
            if (bounds.outWidth <= 0) return null
            var s = 1
            while (bounds.outWidth / s > max || bounds.outHeight / s > max) s *= 2
            BitmapFactory.decodeByteArray(data, 0, data.size, BitmapFactory.Options().apply { inSampleSize = s })
        }.getOrNull()
    }

    /** An incoming auto-applied clip (text or image): a SINGLE, self-replacing,
     *  heads-up popup with a preview that auto-dismisses (toast-like). Reusing one
     *  id means clips never pile up in the shade. */
    fun notifyClip(ctx: Context, origin: String, preview: String, imageBytes: ByteArray? = null) {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        val bmp = thumb(imageBytes)
        val b = Notification.Builder(ctx, POPUP_CHANNEL)
            .setContentTitle("📋 ${deviceName(origin)}")
            .setContentText(preview.take(120))
            .setSmallIcon(R.drawable.ic_stat_sync)
            .setAutoCancel(true)
            .setTimeoutAfter(8000) // disappears on its own; does not linger
            .setContentIntent(openAppPi(ctx))
        if (bmp != null) {
            b.setLargeIcon(bmp).setStyle(Notification.BigPictureStyle().bigPicture(bmp))
        } else {
            b.setStyle(Notification.BigTextStyle().bigText(preview))
        }
        nm.notify(INCOMING_ID, b.build())
    }

    /** A received file/large item needing an explicit save: persistent + tappable,
     *  with the image thumbnail when we already have the bytes. */
    fun notifyDownloadable(
        ctx: Context,
        origin: String,
        blobId: String,
        name: String,
        mime: String,
        encrypted: Boolean,
        imageBytes: ByteArray? = null,
    ) {
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
        val bmp = thumb(imageBytes)
        val b = Notification.Builder(ctx, POPUP_CHANNEL)
            .setContentTitle("📎 ${deviceName(origin)}")
            .setContentText("$name — 탭하여 저장")
            .setSmallIcon(android.R.drawable.stat_sys_download)
            .setAutoCancel(true)
            .setContentIntent(pi)
        if (bmp != null) {
            b.setLargeIcon(bmp).setStyle(Notification.BigPictureStyle().bigPicture(bmp))
        }
        nm.notify(nextDlId++, b.build())
    }
}
