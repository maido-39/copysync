package com.copysync.android.capture

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.graphics.PixelFormat
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/**
 * Gains transient window focus via a 1x1 invisible SYSTEM_ALERT_WINDOW overlay so
 * the app can read/write the clipboard from the background — the workaround used
 * by ClipCascade/KDE Connect. Requires the "display over other apps" permission.
 *
 * Calls block the (background) caller until the overlay gains focus and the
 * action runs, or a timeout elapses; all window/clipboard work happens on the
 * main thread.
 */
class OverlayController(private val context: Context) {
    private val main = Handler(Looper.getMainLooper())
    private val wm = context.getSystemService(WindowManager::class.java)
    private val clipboard = context.getSystemService(ClipboardManager::class.java)

    fun canOverlay(): Boolean = Settings.canDrawOverlays(context)

    fun readWithFocus(timeoutMs: Long = 1200): ClipData? {
        val out = AtomicReference<ClipData?>()
        runFocused(timeoutMs) { out.set(clipboard.primaryClip) }
        return out.get()
    }

    fun writeWithFocus(clip: ClipData, timeoutMs: Long = 1200): Boolean {
        val ok = AtomicReference(false)
        runFocused(timeoutMs) {
            clipboard.setPrimaryClip(clip)
            ok.set(true)
        }
        return ok.get()
    }

    private fun runFocused(timeoutMs: Long, action: () -> Unit) {
        if (!canOverlay()) return
        val latch = CountDownLatch(1)
        main.post {
            val holder = AtomicReference<View?>()
            val remove = Runnable {
                holder.getAndSet(null)?.let { runCatching { wm.removeView(it) } }
                if (latch.count > 0) latch.countDown()
            }
            val view = object : View(context) {
                override fun onWindowFocusChanged(hasWindowFocus: Boolean) {
                    super.onWindowFocusChanged(hasWindowFocus)
                    if (hasWindowFocus) {
                        runCatching { action() }
                        main.post(remove)
                    }
                }
            }
            holder.set(view)
            val params = WindowManager.LayoutParams(
                1, 1,
                WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY,
                WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL, // focusable, but doesn't steal taps
                PixelFormat.TRANSLUCENT,
            ).apply { gravity = Gravity.TOP or Gravity.START }
            runCatching { wm.addView(view, params) }.onFailure { remove.run() }
            main.postDelayed(remove, timeoutMs) // safety net
        }
        latch.await(timeoutMs + 300, TimeUnit.MILLISECONDS)
    }
}
