package com.copysync.android.sync

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import com.copysync.android.capture.ClipboardCaptureEngine
import com.copysync.android.data.ClipEntity
import com.copysync.android.data.HistoryDb
import com.copysync.android.data.Secrets
import com.copysync.android.data.Settings
import com.copysync.android.net.Ack
import com.copysync.android.net.ClipEvent
import com.copysync.android.net.Envelope
import com.copysync.android.net.Hello
import com.copysync.android.net.HelloOk
import com.copysync.android.net.MsgType
import com.copysync.android.net.WsClient
import com.copysync.android.net.decodePayload
import com.copysync.android.net.pinnedClient
import com.copysync.android.net.sha256Hex
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.util.UUID

/**
 * The specialUse foreground service: it hosts the persistent WebSocket session,
 * runs the clipboard capture engine, relays local copies to the server, writes
 * inbound clips back, and records history. specialUse avoids the dataSync 6h cap
 * and the BOOT_COMPLETED restriction.
 */
class SyncService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val settings by lazy { Settings(this) }
    private val secrets by lazy { Secrets(this) }
    private val dao by lazy { HistoryDb.get(this).clipDao() }

    private var capture: ClipboardCaptureEngine? = null
    @Volatile private var ws: WsClient? = null
    private var seq = 0L

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        Notifications.ensureChannels(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForegroundCompat("starting…")
        if (intent?.action == ACTION_STOP || !settings.isPaired) {
            stopSelf()
            return START_NOT_STICKY
        }
        if (!SyncState.running.value) {
            SyncState.running.value = true
            startCapture()
            startConnectLoop()
        }
        return START_STICKY
    }

    private fun startCapture() {
        if (capture != null) return
        capture = ClipboardCaptureEngine(this) { text -> onLocalCopy(text) }.also { it.start() }
    }

    private fun onLocalCopy(text: String) {
        scope.launch {
            val sha = sha256Hex(text)
            dao.insert(
                ClipEntity(
                    clipId = UUID.randomUUID().toString(), ts = System.currentTimeMillis(),
                    direction = "out", origin = settings.deviceId.orEmpty(), text = text, sha = sha,
                ),
            )
            dao.prune(300)
            val sent = ws?.sendClip(
                ClipEvent(
                    id = UUID.randomUUID().toString(), seq = ++seq, mime = listOf("text/plain"),
                    inlineText = text, size = text.toByteArray().size.toLong(), sha256 = sha,
                ),
            )
            SyncState.lastEvent.value = "sent: ${text.take(40)}"
            Log.i("CopySync", "local copy -> sendClip (ws=${ws != null}, sent=$sent, ${text.length} chars)")
        }
    }

    private fun startConnectLoop() {
        scope.launch {
            var backoff = 1000L
            while (isActive && SyncState.running.value) {
                try {
                    val url = settings.serverUrl
                    val pin = settings.spkiPin
                    val token = secrets.token
                    val devId = settings.deviceId
                    if (url == null || pin == null || token == null || devId == null) {
                        update("not paired"); break
                    }
                    val w = WsClient(pinnedClient(pin))
                    ws = w
                    val incomingJob = launch { w.incoming.collect { handle(it) } }
                    val wsUrl = url.replaceFirst("https://", "wss://")
                        .replaceFirst("http://", "ws://").trimEnd('/') + "/ws"
                    update("connecting…")
                    w.connect(wsUrl, object : WsClient.Listener {
                        override fun onOpen() {
                            w.sendHello(
                                Hello(deviceId = devId, deviceName = settings.deviceName ?: "android", token = token),
                            )
                            update("connected")
                        }

                        override fun onClosed(reason: String) {
                            update("disconnected: $reason")
                        }
                    })

                    var waited = 0
                    while (isActive && !w.connected.value && waited < 8000) {
                        delay(200); waited += 200
                    }
                    if (w.connected.value) backoff = 1000L
                    while (isActive && SyncState.running.value && w.connected.value) {
                        SyncState.connected.value = true
                        delay(1000)
                    }
                    SyncState.connected.value = false
                    incomingJob.cancel()
                    w.close()
                } catch (e: Exception) {
                    update("error: ${e.message}")
                    SyncState.connected.value = false
                }
                if (!SyncState.running.value) break
                delay(backoff)
                backoff = (backoff * 2).coerceAtMost(30_000L)
            }
        }
    }

    private suspend fun handle(env: Envelope) {
        when (env.t) {
            MsgType.HELLO_OK -> {
                val ok = runCatching { env.decodePayload<HelloOk>() }.getOrNull()
                update("synced with ${ok?.serverName ?: "server"}")
            }
            MsgType.ACK -> {
                val a = runCatching { env.decodePayload<Ack>() }.getOrNull()
                Log.i("CopySync", "ack ${a?.id}: status=${a?.status} queuedFor=${a?.queuedFor}")
            }
            MsgType.CLIP -> {
                val ev = runCatching { env.decodePayload<ClipEvent>() }.getOrNull() ?: return
                val text = ev.inlineText ?: return // blob payloads handled in a later stage
                dao.insert(
                    ClipEntity(
                        clipId = ev.id, ts = System.currentTimeMillis(), direction = "in",
                        origin = ev.originDeviceId, text = text,
                        sha = ev.sha256.ifEmpty { sha256Hex(text) },
                    ),
                )
                dao.prune(300)
                capture?.applyInbound(text)
                Notifications.notifyClip(this, ev.originDeviceId, text)
                SyncState.lastEvent.value = "received: ${text.take(40)}"
                Log.i("CopySync", "received clip from ${ev.originDeviceId} (${text.length} chars)")
            }
        }
    }

    private fun update(s: String) {
        SyncState.status.value = s
        Log.i("CopySync", "status: $s")
    }

    private fun startForegroundCompat(text: String) {
        val n = Notifications.serviceNotification(this, text)
        when {
            Build.VERSION.SDK_INT >= 34 ->
                startForeground(Notifications.SERVICE_NOTIF_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
            else ->
                startForeground(Notifications.SERVICE_NOTIF_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        }
    }

    override fun onDestroy() {
        SyncState.running.value = false
        SyncState.connected.value = false
        capture?.stop()
        capture = null
        ws?.close()
        scope.cancel()
        super.onDestroy()
    }

    companion object {
        const val ACTION_STOP = "com.copysync.android.STOP"

        fun start(ctx: Context) {
            val i = Intent(ctx, SyncService::class.java)
            if (Build.VERSION.SDK_INT >= 26) ctx.startForegroundService(i) else ctx.startService(i)
        }

        fun stop(ctx: Context) {
            ctx.startService(Intent(ctx, SyncService::class.java).apply { action = ACTION_STOP })
        }
    }
}
