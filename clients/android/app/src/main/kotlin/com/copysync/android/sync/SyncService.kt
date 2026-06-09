package com.copysync.android.sync

import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Base64
import android.util.Log
import com.copysync.android.capture.Captured
import com.copysync.android.capture.ClipboardCaptureEngine
import com.copysync.android.data.ClipEntity
import com.copysync.android.data.HistoryDb
import com.copysync.android.data.Secrets
import com.copysync.android.data.Settings
import com.copysync.android.net.Ack
import com.copysync.android.net.BlobClient
import com.copysync.android.net.BlobRequest
import com.copysync.android.net.ClipEvent
import com.copysync.android.net.DeviceInfo
import com.copysync.android.net.Presence
import com.copysync.android.net.Roster
import com.copysync.android.net.E2eCrypto
import com.copysync.android.net.EncMeta
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
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.io.File
import java.util.UUID
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonPrimitive

/**
 * The specialUse foreground service: it hosts the persistent WebSocket session,
 * runs the clipboard capture engine, relays local copies (text + images) to the
 * server, writes inbound clips back, and records history. specialUse avoids the
 * dataSync 6h cap and the BOOT_COMPLETED restriction.
 */
class SyncService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val settings by lazy { Settings(this) }
    private val secrets by lazy { Secrets(this) }
    private val dao by lazy { HistoryDb.get(this).clipDao() }

    private var capture: ClipboardCaptureEngine? = null
    @Volatile private var ws: WsClient? = null
    @Volatile private var blob: BlobClient? = null
    @Volatile private var threshold: Long = 0 // bytes; files over this go on demand (from hello_ok)
    @Volatile private var key: ByteArray? = null // E2E group key (null = off)
    private var kid: String = ""
    @Volatile private var keyChecked = false
    @Volatile private var clearJob: Job? = null
    private val PREVIEW_CAP_BYTES = 25L * 1024 * 1024 // images up to this are fetched to show a history thumbnail
    private var seq = 0L

    /** Build the outbound `targets` field from the user's routing selection. */
    private fun currentTargets(): JsonElement {
        val sel = SyncState.targets.value
        return if (sel.isEmpty()) JsonPrimitive("all")
        else JsonArray(sel.map { JsonPrimitive(it) })
    }

    private fun scheduleAutoClear(sha: String) {
        val sec = settings.autoClearSeconds
        if (sec <= 0) return
        clearJob?.cancel()
        clearJob = scope.launch {
            delay(sec * 1000L)
            capture?.clearIfStill(sha)
            DebugLog.i("auto-clear fired (${sec}s)")
        }
    }

    /** Derive the E2E key from the stored passphrase + serverId once (Argon2 is slow). */
    private fun ensureKey() {
        if (keyChecked) return
        keyChecked = true
        val pass = secrets.e2ePass
        val sid = settings.serverId
        if (!pass.isNullOrEmpty() && !sid.isNullOrEmpty()) {
            key = E2eCrypto.deriveKey(pass, sid)
            kid = E2eCrypto.keyId(key!!)
            DebugLog.i("E2E enabled (keyId=$kid)")
        }
    }

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
        if (intent?.action == ACTION_SHARE_FILE) {
            handleShare(intent)
        }
        if (intent?.action == ACTION_SET_POOL) {
            intent.getStringExtra(EXTRA_POOL)?.let { p ->
                ws?.setPool(p)
                SyncState.currentPool.value = p
                DebugLog.i("pool -> $p")
                refreshNotification()
            }
        }
        return START_STICKY
    }

    private fun handleShare(intent: Intent) {
        val sha = intent.getStringExtra("sha") ?: return
        val name = intent.getStringExtra("name") ?: "file"
        val mime = intent.getStringExtra("mime") ?: "application/octet-stream"
        val size = intent.getLongExtra("size", 0)
        val file = File(File(cacheDir, "clip-src"), sha)
        if (!file.exists()) {
            DebugLog.w("shared file missing: $sha")
            return
        }
        DebugLog.i("share → sending $name ($size bytes)")
        onCaptured(Captured.Binary(file, mime, name, size, sha))
    }

    private fun startCapture() {
        if (capture != null) return
        capture = ClipboardCaptureEngine(this) { onCaptured(it) }.also { it.start() }
    }

    private fun onCaptured(c: Captured) {
        scope.launch {
            when (c) {
                is Captured.Text -> {
                    ensureKey()
                    val sha = sha256Hex(c.text)
                    dao.insert(
                        ClipEntity(
                            clipId = UUID.randomUUID().toString(), ts = System.currentTimeMillis(),
                            direction = "out", origin = settings.deviceId.orEmpty(), text = c.text, sha = sha,
                            enc = key != null,
                        ),
                    )
                    var inline = c.text
                    var wireSha = sha
                    var encMeta: EncMeta? = null
                    key?.let { k ->
                        val raw = E2eCrypto.seal(k, c.text.toByteArray())
                        inline = Base64.encodeToString(raw, Base64.NO_WRAP)
                        wireSha = sha256Hex(raw)
                        encMeta = EncMeta("aes-256-gcm", kid)
                    }
                    val htmlField: String? = c.html?.takeIf { it.isNotEmpty() }?.let { h ->
                        key?.let { k -> Base64.encodeToString(E2eCrypto.seal(k, h.toByteArray()), Base64.NO_WRAP) } ?: h
                    }
                    val mimes = if (htmlField != null) listOf("text/html", "text/plain") else listOf("text/plain")
                    val sent = ws?.sendClip(
                        ClipEvent(
                            id = UUID.randomUUID().toString(), seq = ++seq, mime = mimes,
                            inlineText = inline, html = htmlField,
                            size = c.text.toByteArray().size.toLong(), sha256 = wireSha, enc = encMeta,
                            targets = currentTargets(),
                        ),
                    )
                    SyncState.lastEvent.value = "↑ ${c.text.take(40)}"
                    DebugLog.i("local copy -> sendClip (ws=${ws != null}, sent=$sent, e2e=${key != null})")
                }
                is Captured.Binary -> {
                    ensureKey()
                    if (blob == null) {
                        DebugLog.w("no blob channel; file not sent")
                    } else {
                        val kind = if (c.mime.startsWith("image/")) "image" else "file"
                        val dev = settings.deviceId.orEmpty()
                        val k = key
                        // Keep a local PLAINTEXT preview for the history thumbnail, addressed by the
                        // advertised blobId. clip-src can't serve this: it holds ciphertext (E2E
                        // on-demand) or is deleted right after upload, so previews of what we just
                        // sent would always come up empty.
                        fun stashThumb(blobId: String) {
                            runCatching {
                                val td = File(cacheDir, "clip-thumb").apply { mkdirs() }
                                val dst = File(td, blobId.removePrefix("sha256:"))
                                when {
                                    kind == "image" -> c.file.copyTo(dst, overwrite = true)
                                    c.mime.startsWith("video/") -> {
                                        val mmr = android.media.MediaMetadataRetriever()
                                        try {
                                            mmr.setDataSource(c.file.absolutePath)
                                            mmr.getFrameAtTime(0)?.let { bmp ->
                                                java.io.FileOutputStream(dst).use {
                                                    bmp.compress(android.graphics.Bitmap.CompressFormat.PNG, 90, it)
                                                }
                                            }
                                        } finally { mmr.release() }
                                    }
                                    else -> return@runCatching
                                }
                                val files = td.listFiles()?.sortedBy { it.lastModified() } ?: return@runCatching
                                var total = files.sumOf { it.length() }; var i = 0
                                while (total > 120L * 1024 * 1024 && i < files.size) {
                                    total -= files[i].length(); files[i].delete(); i++
                                }
                            }
                        }
                        if (k != null) {
                            // E2E: seal to ciphertext; address + advertise by the ciphertext hash.
                            val ct = E2eCrypto.seal(k, c.file.readBytes())
                            val ctSha = sha256Hex(ct)
                            val blobId = "sha256:$ctSha"
                            stashThumb(blobId)
                            val em = EncMeta("aes-256-gcm", kid)
                            if (threshold > 0 && c.size > threshold) {
                                runCatching { File(File(cacheDir, "clip-src").apply { mkdirs() }, ctSha).writeBytes(ct) }
                                ws?.sendClip(ClipEvent(id = UUID.randomUUID().toString(), seq = ++seq, mime = listOf(c.mime), name = c.name, blobId = blobId, onDemand = true, size = c.size, sha256 = ctSha, enc = em, targets = currentTargets()))
                            } else {
                                if (runCatching { blob?.put(ct) }.isFailure) { DebugLog.w("e2e upload failed"); return@launch }
                                ws?.sendClip(ClipEvent(id = UUID.randomUUID().toString(), seq = ++seq, mime = listOf(c.mime), name = c.name, blobId = blobId, onDemand = false, size = c.size, sha256 = ctSha, enc = em, targets = currentTargets()))
                            }
                            c.file.delete()
                            dao.insert(fileEntity(UUID.randomUUID().toString(), "out", dev, ctSha, kind, blobId, c.name, c.size, c.mime, enc = true))
                            SyncState.lastEvent.value = "↑ ${c.name} (e2e ${c.size / 1024}KB)"
                            DebugLog.i("sent e2e ${c.name} ${c.size} bytes ($blobId)")
                        } else {
                            val blobId = "sha256:" + c.sha
                            stashThumb(blobId)
                            if (threshold > 0 && c.size > threshold) {
                                ws?.sendClip(ClipEvent(id = UUID.randomUUID().toString(), seq = ++seq, mime = listOf(c.mime), name = c.name, blobId = blobId, onDemand = true, size = c.size, sha256 = c.sha, targets = currentTargets()))
                                dao.insert(fileEntity(UUID.randomUUID().toString(), "out", dev, c.sha, kind, blobId, c.name, c.size, c.mime))
                                SyncState.lastEvent.value = "↑ ${c.name} (on-demand ${c.size / 1024}KB)"
                            } else {
                                if (runCatching { blob?.putFile(c.file, c.mime, blobId) }.isFailure) { DebugLog.w("file upload failed: ${c.name}"); return@launch }
                                ws?.sendClip(ClipEvent(id = UUID.randomUUID().toString(), seq = ++seq, mime = listOf(c.mime), name = c.name, blobId = blobId, onDemand = false, size = c.size, sha256 = c.sha, targets = currentTargets()))
                                dao.insert(fileEntity(UUID.randomUUID().toString(), "out", dev, c.sha, kind, blobId, c.name, c.size, c.mime))
                                c.file.delete()
                                SyncState.lastEvent.value = "↑ ${c.name} (${c.size / 1024}KB)"
                            }
                        }
                    }
                }
            }
            dao.prune(300)
            refreshNotification()
        }
    }

    private fun fileEntity(
        clipId: String, dir: String, origin: String, sha: String,
        kind: String, blobId: String, name: String, size: Long, mime: String, enc: Boolean = false,
    ) = ClipEntity(
        clipId = clipId, ts = System.currentTimeMillis(), direction = dir, origin = origin,
        text = "($kind) $name", sha = sha, kind = kind, blobId = blobId, name = name, sizeBytes = size, mime = mime, enc = enc,
    )

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
                    val client = pinnedClient(pin)
                    val w = WsClient(client)
                    ws = w
                    blob = BlobClient(client, url, token)
                    val incomingJob = launch { w.incoming.collect { handle(it) } }
                    val wsUrl = url.replaceFirst("https://", "wss://")
                        .replaceFirst("http://", "ws://").trimEnd('/') + "/ws"
                    update("connecting…")
                    w.connect(wsUrl, object : WsClient.Listener {
                        override fun onOpen() {
                            SyncState.connected.value = true
                            w.sendHello(
                                Hello(deviceId = devId, deviceName = settings.deviceName ?: "android", token = token),
                            )
                            update("connected")
                        }

                        override fun onClosed(reason: String) {
                            SyncState.connected.value = false
                            update("disconnected: $reason")
                        }
                    })

                    var waited = 0
                    while (isActive && !w.connected.value && waited < 8000) {
                        delay(200); waited += 200
                    }
                    if (w.connected.value) backoff = 1000L
                    while (isActive && SyncState.running.value && w.connected.value) {
                        delay(1000)
                    }
                    SyncState.connected.value = false
                    incomingJob.cancel()
                    w.close()
                    blob = null
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
                threshold = ok?.onDemandThreshold ?: 0
                settings.onDemandThreshold = threshold
                SyncState.roster.value = ok?.roster ?: emptyList()
                SyncState.pools.value = ok?.pools?.ifEmpty { listOf("default") } ?: listOf("default")
                SyncState.currentPool.value = ok?.pool?.ifEmpty { "default" } ?: "default"
                update("synced with ${ok?.serverName ?: "server"}")
                refreshNotification()
            }
            MsgType.ACK -> {
                val a = runCatching { env.decodePayload<Ack>() }.getOrNull()
                DebugLog.i("ack ${a?.id}: status=${a?.status} queuedFor=${a?.queuedFor}")
            }
            MsgType.ROSTER -> {
                val r = runCatching { env.decodePayload<Roster>() }.getOrNull()
                if (r != null) SyncState.roster.value = r.devices
            }
            MsgType.PRESENCE -> {
                val p = runCatching { env.decodePayload<Presence>() }.getOrNull() ?: return
                val cur = SyncState.roster.value.toMutableList()
                val i = cur.indexOfFirst { it.id == p.device.id }
                val d: DeviceInfo = p.device.copy(online = p.online)
                if (i >= 0) cur[i] = d else cur.add(d)
                SyncState.roster.value = cur
            }
            MsgType.BLOB_REQUEST -> {
                val br = runCatching { env.decodePayload<BlobRequest>() }.getOrNull() ?: return
                val f = capture?.heldFile(br.id)
                if (f == null) {
                    DebugLog.w("blob_request for unheld blob ${br.id}")
                } else {
                    runCatching { blob?.putFile(f, "application/octet-stream", br.id) }
                        .onSuccess { DebugLog.i("served on demand: ${br.id} (${f.length()} bytes)") }
                        .onFailure { DebugLog.w("serve failed: ${it.message}") }
                }
            }
            MsgType.CLIP -> {
                val ev = runCatching { env.decodePayload<ClipEvent>() }.getOrNull() ?: return
                val imageMime = ev.mime.firstOrNull { it.startsWith("image/") }
                when {
                    ev.inlineText != null -> {
                        ensureKey()
                        var text = ev.inlineText!!
                        var readable = true
                        if (ev.enc != null) {
                            val k = key
                            val dec = if (k != null && (ev.enc!!.keyId.isEmpty() || ev.enc!!.keyId == kid))
                                runCatching { String(E2eCrypto.open(k, Base64.decode(text, Base64.NO_WRAP))) }.getOrNull() else null
                            if (dec != null) text = dec else { text = "[encrypted — no matching key]"; readable = false }
                        }
                        dao.insert(
                            ClipEntity(
                                clipId = ev.id, ts = System.currentTimeMillis(), direction = "in",
                                origin = ev.originDeviceId, text = text, sha = ev.sha256.ifEmpty { sha256Hex(text) }, enc = ev.enc != null,
                            ),
                        )
                        if (readable) {
                            var html: String? = ev.html?.takeIf { it.isNotEmpty() }
                            if (html != null && ev.enc != null) {
                                val k = key
                                html = if (k != null) runCatching { String(E2eCrypto.open(k, Base64.decode(html, Base64.NO_WRAP))) }.getOrNull() else null
                            }
                            capture?.applyInbound(text, html, settings.sensitiveMark)
                            scheduleAutoClear(sha256Hex(text))
                        }
                        Notifications.notifyClip(this, ev.originDeviceId, text)
                        SyncState.lastEvent.value = "↓ ${text.take(40)}"
                        DebugLog.i("received text (e2e=${ev.enc != null}, readable=$readable)")
                    }
                    ev.blobId != null && imageMime != null && (!ev.onDemand || ev.size in 1L..PREVIEW_CAP_BYTES) -> {
                        // Any image (eager, or on-demand within the cap) is fetched + decrypted +
                        // cached locally so the history list can render a real thumbnail.
                        ensureKey()
                        var data = runCatching { blob?.get(ev.blobId!!) }.getOrNull()
                        if (data != null && ev.enc != null) {
                            val k = key
                            data = if (k != null) runCatching { E2eCrypto.open(k, data!!) }.getOrNull() else null
                        }
                        val ext = imageMime.substringAfter('/').substringBefore(';').ifEmpty { "img" }
                        val nm = ev.name ?: "image.$ext"
                        if (data != null) {
                            runCatching { // decrypted preview cache (device-only), keyed by blob sha
                                File(File(cacheDir, "clip-src").apply { mkdirs() }, ev.blobId!!.removePrefix("sha256:")).writeBytes(data!!)
                            }
                            if (!ev.onDemand) {
                                capture?.applyInboundImage(data!!, nm, settings.sensitiveMark)
                                scheduleAutoClear(sha256Hex(data!!))
                                dao.insert(fileEntity(ev.id, "in", ev.originDeviceId, ev.sha256.ifEmpty { sha256Hex(data!!) }, "image", ev.blobId!!, nm, data!!.size.toLong(), imageMime, enc = ev.enc != null))
                                Notifications.notifyClip(this, ev.originDeviceId, "(image)")
                            } else {
                                // on-demand: thumbnail is cached, but let the user save it explicitly
                                dao.insert(fileEntity(ev.id, "in", ev.originDeviceId, ev.sha256, "image", ev.blobId!!, nm, ev.size, imageMime, enc = ev.enc != null))
                                Notifications.notifyDownloadable(this, ev.originDeviceId, ev.blobId!!, nm, imageMime, ev.enc != null)
                            }
                            SyncState.lastEvent.value = "↓ image ${data!!.size / 1024}KB"
                            DebugLog.i("received image ${data!!.size} bytes (onDemand=${ev.onDemand}, e2e=${ev.enc != null})")
                        } else {
                            dao.insert(fileEntity(ev.id, "in", ev.originDeviceId, ev.sha256, "image", ev.blobId!!, nm, ev.size, imageMime, enc = ev.enc != null))
                            Notifications.notifyDownloadable(this, ev.originDeviceId, ev.blobId!!, nm, imageMime, ev.enc != null)
                            DebugLog.w("inbound image: fetch/decrypt failed for ${ev.blobId}")
                        }
                    }
                    ev.blobId != null -> {
                        // Non-image file, or a large on-demand image → record as downloadable.
                        val nm = ev.name ?: "file"
                        val kind = if (imageMime != null) "image" else "file"
                        dao.insert(fileEntity(ev.id, "in", ev.originDeviceId, ev.sha256, kind, ev.blobId!!, nm, ev.size, ev.mime.firstOrNull() ?: "", enc = ev.enc != null))
                        Notifications.notifyDownloadable(this, ev.originDeviceId, ev.blobId!!, nm, ev.mime.firstOrNull() ?: "*/*", ev.enc != null)
                        SyncState.lastEvent.value = "↓ $nm (download)"
                        DebugLog.i("received file metadata: $nm (${ev.size} bytes, onDemand=${ev.onDemand}, e2e=${ev.enc != null})")
                    }
                    else -> DebugLog.i("ignoring unsupported clip (mime=${ev.mime})")
                }
                dao.prune(300)
                refreshNotification()
            }
        }
    }

    private fun update(s: String) {
        SyncState.status.value = s
        DebugLog.i("status: $s")
        refreshNotification()
    }

    private fun refreshNotification() {
        val text = SyncState.lastEvent.value.ifEmpty { SyncState.status.value }
        runCatching {
            getSystemService(NotificationManager::class.java).notify(
                Notifications.SERVICE_NOTIF_ID,
                Notifications.serviceNotification(this, text, SyncState.connected.value),
            )
        }
    }

    private fun startForegroundCompat(text: String) {
        val n = Notifications.serviceNotification(this, text, SyncState.connected.value)
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
        const val ACTION_SHARE_FILE = "com.copysync.android.SHARE_FILE"
        const val ACTION_SET_POOL = "com.copysync.android.SET_POOL"
        const val EXTRA_POOL = "pool"

        fun start(ctx: Context) {
            val i = Intent(ctx, SyncService::class.java)
            if (Build.VERSION.SDK_INT >= 26) ctx.startForegroundService(i) else ctx.startService(i)
        }

        /** Switch this device's share pool (from the app or a notification action). */
        fun setPool(ctx: Context, pool: String) {
            val i = Intent(ctx, SyncService::class.java).apply {
                action = ACTION_SET_POOL
                putExtra(EXTRA_POOL, pool)
            }
            if (Build.VERSION.SDK_INT >= 26) ctx.startForegroundService(i) else ctx.startService(i)
        }

        /** Send a file already cached in clip-src/<sha> (used by the Share target). */
        fun shareFile(ctx: Context, sha: String, name: String, mime: String, size: Long) {
            val i = Intent(ctx, SyncService::class.java).apply {
                action = ACTION_SHARE_FILE
                putExtra("sha", sha)
                putExtra("name", name)
                putExtra("mime", mime)
                putExtra("size", size)
            }
            if (Build.VERSION.SDK_INT >= 26) ctx.startForegroundService(i) else ctx.startService(i)
        }

        fun stop(ctx: Context) {
            ctx.startService(Intent(ctx, SyncService::class.java).apply { action = ACTION_STOP })
        }
    }
}
