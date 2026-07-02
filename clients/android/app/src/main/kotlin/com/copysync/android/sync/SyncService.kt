package com.copysync.android.sync

import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Base64
import android.util.Log
import com.copysync.android.capture.Captured
import com.copysync.android.capture.ClipboardCaptureEngine
import com.copysync.android.data.ClipEntity
import com.copysync.android.data.HistoryCrypto
import com.copysync.android.data.HistoryDb
import com.copysync.android.data.Secrets
import com.copysync.android.data.Settings
import com.copysync.android.net.Ack
import com.copysync.android.net.BlobClient
import com.copysync.android.net.BlobRequest
import com.copysync.android.net.ClipEvent
import com.copysync.android.net.DeviceInfo
import com.copysync.android.net.Presence
import com.copysync.android.net.Privacy
import com.copysync.android.net.Roster
import com.copysync.android.net.TokenRotate
import com.copysync.android.net.E2eCrypto
import com.copysync.android.net.EncMeta
import com.copysync.android.net.Envelope
import com.copysync.android.net.Hello
import com.copysync.android.net.HelloErr
import com.copysync.android.net.HelloOk
import com.copysync.android.net.MsgType
import com.copysync.android.net.WsClient
import com.copysync.android.net.decodePayload
import com.copysync.android.net.pinnedClient
import com.copysync.android.net.sha256Hex
import kotlinx.coroutines.CancellationException
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
    // Separate blob channel for INBOUND on-demand preview pulls: the server
    // broker-pulls the bytes from the origin device (up to ~60s), which exceeds
    // the default 10s read timeout the shared `client` uses for the WS + eager
    // ops. Built with a 120s timeout, matching Downloads.kt, so large/slow
    // previews don't spuriously time out and get downgraded to download-on-tap.
    @Volatile private var blobPull: BlobClient? = null
    // The pinned OkHttpClient is reused across reconnects (idiomatic OkHttp: one
    // client owns a Dispatcher executor + ConnectionPool). Rebuilt only when the
    // pin changes (re-pair). Reusing it avoids leaking a Dispatcher/ConnectionPool
    // per reconnect attempt and lets the connection pool aid reconnects.
    @Volatile private var httpClient: okhttp3.OkHttpClient? = null
    @Volatile private var httpPullClient: okhttp3.OkHttpClient? = null
    private var httpClientPin: String? = null

    // P0 — NETWORK BINDING. The socket is otherwise unbound: a Wi-Fi↔cellular
    // switch makes OkHttp redial over cellular against the LAN-only server →
    // SocketTimeout from the clatd 192.0.0.2 464XLAT source. We register a
    // ConnectivityManager callback for a Wi-Fi+INTERNET network, thread the
    // chosen Network's socketFactory into the pinned client so every dial pins to
    // the Wi-Fi/LAN interface, and force a clean WS rebuild on availability change.
    private var connectivityMgr: ConnectivityManager? = null
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    @Volatile private var boundNetwork: Network? = null
    // The pinned client is rebuilt when the bound Network changes (its socketFactory
    // is baked in at build time). Compared alongside the pin in the connect loop.
    private var httpClientNetwork: Network? = null
    // Bumped on every network change so the connect loop's inner wait/session loop
    // exits promptly and rebuilds on the new interface instead of blocking up to a
    // full backoff cycle on the dead one.
    private val netGeneration = java.util.concurrent.atomic.AtomicInteger(0)
    @Volatile private var threshold: Long = 0 // bytes; files over this go on demand (from hello_ok)
    @Volatile private var key: ByteArray? = null // E2E group key (null = off)
    private var kid: String = ""
    @Volatile private var keyChecked = false
    @Volatile private var clearJob: Job? = null
    private val PREVIEW_CAP_BYTES = 25L * 1024 * 1024 // images up to this are fetched to show a history thumbnail
    // Atomic: outbound ClipEvents are built from concurrently-launched coroutines
    // on Dispatchers.IO, so the increment must be a single atomic op (not a plain
    // read-modify-write on a non-volatile Long).
    private val seq = java.util.concurrent.atomic.AtomicLong(0)

    // P1 — OUTBOUND RETRY QUEUE. Clips captured while disconnected, or whose send
    // fails (blob upload / not-connected), used to be dropped with only a debug
    // line. We now enqueue the ORIGINAL Captured item into a bounded in-memory
    // queue and re-run the full send pipeline in order once (re)connected (after
    // hello_ok). Sensitive-filtered clips are never enqueued (they return before
    // reaching here). Oldest is dropped when the cap is exceeded.
    private val OUTBOUND_QUEUE_CAP = 50
    private val pendingOutbound = java.util.concurrent.ConcurrentLinkedDeque<Captured>()
    // Guards the flush so a reconnect racing with an in-progress flush can't run
    // two drains concurrently (which would double-send).
    @Volatile private var flushing = false

    /**
     * Enqueue a clip that could not be sent (offline / blob channel down / send
     * failure), bounded + drop-oldest. Sensitive clips must be filtered BEFORE
     * calling this — never enqueue them.
     */
    private fun enqueueOutbound(c: Captured, why: String) {
        pendingOutbound.addLast(c)
        while (pendingOutbound.size > OUTBOUND_QUEUE_CAP) {
            pendingOutbound.pollFirst()
            DebugLog.w("outbound queue full (cap=$OUTBOUND_QUEUE_CAP) — dropped oldest pending clip")
        }
        DebugLog.i("queued outbound clip ($why); pending=${pendingOutbound.size}")
    }

    /** Flush queued outbound clips in FIFO order after a (re)connect + hello_ok. */
    private fun flushOutbound() {
        if (flushing) return
        if (pendingOutbound.isEmpty()) return
        flushing = true
        scope.launch {
            try {
                // Only drain the items present at flush start. Items that re-fail
                // re-enqueue to the TAIL; capping the drain to the initial count
                // prevents an immediate hot re-retry spin on a persistently-failing
                // clip (it waits for the next reconnect/flush instead).
                var remaining = pendingOutbound.size
                DebugLog.i("flushing $remaining queued outbound clip(s)")
                while (remaining-- > 0 && SyncState.connected.value) {
                    val c = pendingOutbound.pollFirst() ?: break
                    sendCaptured(c, fromQueue = true)
                }
            } finally {
                flushing = false
            }
        }
    }

    // Doze hardening: a CPU partial wakelock + a high-perf Wi-Fi lock keep the
    // persistent socket alive while the screen is off. Held for the service
    // lifetime; both released in onDestroy.
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null

    /** Coarse network type for the verbose log (wifi / cellular / ethernet / none). */
    private fun networkType(): String {
        return runCatching {
            val cm = getSystemService(ConnectivityManager::class.java) ?: return "unknown"
            val caps = cm.getNetworkCapabilities(cm.activeNetwork) ?: return "none"
            when {
                caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> "wifi"
                caps.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> "cellular"
                caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> "ethernet"
                caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN) -> "vpn"
                else -> "other"
            }
        }.getOrDefault("unknown")
    }

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
        DebugLog.v("svc") { "onCreate" }
        Notifications.ensureChannels(this)
        acquireLocks()
        registerNetworkCallback()
    }

    /**
     * P0 — bind the socket to the Wi-Fi/LAN interface. Request a network with
     * TRANSPORT_WIFI + INTERNET capability and remember it as [boundNetwork]; the
     * connect loop threads its socketFactory into the pinned client so every dial
     * leaves that interface (never the clatd 192.0.0.2 cellular path against the
     * LAN-only server). onAvailable/onLost bump [netGeneration] + kick the ws so a
     * clean rebuild happens on the correct interface. minSdk 29 → callbacks are
     * always available; guarded defensively anyway.
     */
    private fun registerNetworkCallback() {
        runCatching {
            val cm = getSystemService(ConnectivityManager::class.java) ?: return
            connectivityMgr = cm
            val req = NetworkRequest.Builder()
                .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
                .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                .build()
            val cb = object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) {
                    boundNetwork = network
                    netGeneration.incrementAndGet()
                    DebugLog.i("network available (wifi) → binding socket + forcing reconnect")
                    forceReconnect("network available")
                }

                override fun onLost(network: Network) {
                    if (boundNetwork == network) {
                        boundNetwork = null
                        netGeneration.incrementAndGet()
                        DebugLog.w("bound network lost → will rebuild on next available interface")
                        forceReconnect("network lost")
                    }
                }
            }
            networkCallback = cb
            // requestNetwork keeps a Wi-Fi network up (and reports the specific one);
            // registerDefaultNetworkCallback would follow the system default even when
            // it flips to cellular, which is exactly what we want to avoid here.
            cm.requestNetwork(req, cb)
            DebugLog.v("svc") { "NetworkCallback registered (TRANSPORT_WIFI + INTERNET)" }
        }.onFailure { DebugLog.e("svc", "registerNetworkCallback failed", it) }
    }

    /** Tear the current WS down so the connect loop redials immediately on the
     *  freshly-chosen interface (rather than waiting out a backoff on a dead one). */
    private fun forceReconnect(why: String) {
        if (!SyncState.running.value) return
        DebugLog.v("svc") { "forceReconnect ($why)" }
        runCatching { ws?.close() }
    }

    /** Acquire a PARTIAL_WAKE_LOCK + a high-perf Wi-Fi lock for the service lifetime. */
    private fun acquireLocks() {
        runCatching {
            val pm = getSystemService(PowerManager::class.java)
            wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "copysync:sync").apply {
                setReferenceCounted(false)
                acquire()
            }
            DebugLog.v("wakelock") { "PARTIAL_WAKE_LOCK acquired (copysync:sync)" }
        }.onFailure { DebugLog.e("wakelock", "wakelock acquire failed", it) }
        runCatching {
            val wm = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            @Suppress("DEPRECATION")
            val mode = WifiManager.WIFI_MODE_FULL_HIGH_PERF
            wifiLock = wm.createWifiLock(mode, "copysync:wifi").apply {
                setReferenceCounted(false)
                acquire()
            }
            DebugLog.v("wakelock") { "WifiLock acquired (mode=$mode)" }
        }.onFailure { DebugLog.e("wakelock", "WifiLock acquire failed", it) }
    }

    private fun releaseLocks() {
        runCatching { if (wakeLock?.isHeld == true) { wakeLock?.release(); DebugLog.v("wakelock") { "PARTIAL_WAKE_LOCK released" } } }
            .onFailure { DebugLog.e("wakelock", "wakelock release failed", it) }
        runCatching { if (wifiLock?.isHeld == true) { wifiLock?.release(); DebugLog.v("wakelock") { "WifiLock released" } } }
            .onFailure { DebugLog.e("wakelock", "WifiLock release failed", it) }
        wakeLock = null
        wifiLock = null
    }

    /**
     * On the first paired run, prompt the user to exempt CopySync from battery
     * optimization (the #1 lever against Doze killing the socket). Guarded by
     * isIgnoringBatteryOptimizations + a one-shot pref so we never nag.
     */
    private fun maybePromptBatteryOptimization() {
        if (settings.batteryOptAsked) return
        val pm = getSystemService(PowerManager::class.java) ?: return
        if (pm.isIgnoringBatteryOptimizations(packageName)) {
            settings.batteryOptAsked = true
            DebugLog.v("svc") { "already exempt from battery optimization" }
            return
        }
        settings.batteryOptAsked = true
        runCatching {
            val intent = Intent(android.provider.Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS).apply {
                data = android.net.Uri.parse("package:$packageName")
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            }
            startActivity(intent)
            DebugLog.v("svc") { "prompted ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS" }
        }.onFailure { DebugLog.e("svc", "battery-optimization prompt failed", it) }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        DebugLog.v("svc") { "onStartCommand action=${intent?.action ?: "none"} flags=$flags startId=$startId net=${networkType()}" }
        startForegroundCompat("starting…")
        if (intent?.action == ACTION_STOP || !settings.isPaired) {
            DebugLog.v("svc") { "stopSelf (stop action or not paired)" }
            stopSelf()
            return START_NOT_STICKY
        }
        if (!SyncState.running.value) {
            SyncState.running.value = true
            maybePromptBatteryOptimization()
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
        // Sensitive-filter FIRST, on capture, so a filtered clip is never persisted
        // to history nor enqueued into the retry queue. Non-sensitive clips proceed
        // to sendCaptured (which enqueues on failure/offline).
        if (c is Captured.Text && settings.excludeSensitive) {
            val sens = Privacy.classify(c.text)
            if (sens != null) {
                SyncState.lastEvent.value = "🔒 민감(${sens.label}) — 동기화 안 함"
                SyncState.blockedToast.value =
                    BlockedClip(sens.label, c.text.take(90), System.currentTimeMillis())
                DebugLog.i("sensitive clip filtered (not synced): ${sens.label}")
                DebugLog.v("capture") { "SKIP send: sensitive (${sens.label})" }
                return
            }
        }
        scope.launch { sendCaptured(c, fromQueue = false) }
    }

    /**
     * Run the full outbound send pipeline for one captured clip. On a
     * disconnected/failed send the clip is enqueued for retry (P1) instead of
     * being dropped. Sensitive clips are already filtered out in onCaptured.
     * [fromQueue] items re-enqueue to the TAIL on failure so a permanently-failing
     * head does not block the rest of the drain.
     */
    private suspend fun sendCaptured(c: Captured, fromQueue: Boolean) {
        run {
            when (c) {
                is Captured.Text -> {
                    ensureKey()
                    val sha = sha256Hex(c.text)
                    DebugLog.v("capture") { "text captured: ${c.text.toByteArray().size} bytes, sha=${sha.take(12)}, html=${c.html != null}" }
                    if (!fromQueue) dao.insert(
                        ClipEntity(
                            clipId = UUID.randomUUID().toString(), ts = System.currentTimeMillis(),
                            direction = "out", origin = settings.deviceId.orEmpty(),
                            text = HistoryCrypto.seal(this@SyncService, c.text), sha = sha,
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
                            id = UUID.randomUUID().toString(), seq = seq.incrementAndGet(), mime = mimes,
                            inlineText = inline, html = htmlField,
                            size = c.text.toByteArray().size.toLong(), sha256 = wireSha, enc = encMeta,
                            targets = currentTargets(),
                        ),
                    )
                    if (sent != true) {
                        // Not connected (ws null) or the frame couldn't be enqueued
                        // on the socket: queue for retry after (re)connect instead of
                        // dropping. Enqueue the ORIGINAL plaintext Captured (re-sealed
                        // on flush) so E2E/key changes stay correct.
                        enqueueOutbound(c, "text send failed (ws=${ws != null})")
                        return
                    }
                    SyncState.lastEvent.value = "↑ ${c.text.take(40)}"
                    DebugLog.i("local copy -> sendClip (ws=${ws != null}, sent=$sent, e2e=${key != null})")
                }
                is Captured.Binary -> {
                    ensureKey()
                    DebugLog.v("capture") { "binary captured: ${c.name} kind=${if (c.mime.startsWith("image/")) "image" else "file"} mime=${c.mime} size=${c.size} sha=${c.sha.take(12)}" }
                    if (blob == null || ws == null) {
                        // No live blob channel / socket (down or connecting): queue the
                        // clip for retry after (re)connect instead of dropping it. This
                        // is the most common "my file didn't sync" cause.
                        DebugLog.w("no blob channel for '${c.name}' (${c.size} bytes, connected=${SyncState.connected.value}, net=${networkType()}) — queuing for retry")
                        enqueueOutbound(c, "no blob channel")
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
                                ws?.sendClip(ClipEvent(id = UUID.randomUUID().toString(), seq = seq.incrementAndGet(), mime = listOf(c.mime), name = c.name, blobId = blobId, onDemand = true, size = c.size, sha256 = ctSha, enc = em, targets = currentTargets()))
                            } else {
                                DebugLog.v("blob") { "put attempt (e2e ciphertext, ${ct.size} bytes, $blobId)" }
                                val r = runCatching { blob?.put(ct) }
                                if (r.isFailure) {
                                    DebugLog.w("e2e blob.put failed for '${c.name}' (${c.size} bytes, $blobId) — queuing for retry: ${r.exceptionOrNull()?.message}")
                                    enqueueOutbound(c, "e2e blob.put failed")
                                    return
                                }
                                DebugLog.v("blob") { "put result OK (e2e $blobId)" }
                                ws?.sendClip(ClipEvent(id = UUID.randomUUID().toString(), seq = seq.incrementAndGet(), mime = listOf(c.mime), name = c.name, blobId = blobId, onDemand = false, size = c.size, sha256 = ctSha, enc = em, targets = currentTargets()))
                            }
                            c.file.delete()
                            dao.insert(fileEntity(UUID.randomUUID().toString(), "out", dev, ctSha, kind, blobId, c.name, c.size, c.mime, enc = true))
                            SyncState.lastEvent.value = "↑ ${c.name} (e2e ${c.size / 1024}KB)"
                            DebugLog.i("sent e2e ${c.name} ${c.size} bytes ($blobId)")
                        } else {
                            val blobId = "sha256:" + c.sha
                            stashThumb(blobId)
                            if (threshold > 0 && c.size > threshold) {
                                ws?.sendClip(ClipEvent(id = UUID.randomUUID().toString(), seq = seq.incrementAndGet(), mime = listOf(c.mime), name = c.name, blobId = blobId, onDemand = true, size = c.size, sha256 = c.sha, targets = currentTargets()))
                                dao.insert(fileEntity(UUID.randomUUID().toString(), "out", dev, c.sha, kind, blobId, c.name, c.size, c.mime))
                                SyncState.lastEvent.value = "↑ ${c.name} (on-demand ${c.size / 1024}KB)"
                            } else {
                                DebugLog.v("blob") { "putFile attempt (${c.name}, ${c.size} bytes, $blobId)" }
                                val r = runCatching { blob?.putFile(c.file, c.mime, blobId) }
                                if (r.isFailure) {
                                    DebugLog.w("blob.putFile failed for '${c.name}' (${c.size} bytes, $blobId) — queuing for retry: ${r.exceptionOrNull()?.message}")
                                    enqueueOutbound(c, "blob.putFile failed")
                                    return
                                }
                                DebugLog.v("blob") { "putFile result OK ($blobId)" }
                                ws?.sendClip(ClipEvent(id = UUID.randomUUID().toString(), seq = seq.incrementAndGet(), mime = listOf(c.mime), name = c.name, blobId = blobId, onDemand = false, size = c.size, sha256 = c.sha, targets = currentTargets()))
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
        text = HistoryCrypto.seal(this@SyncService, "($kind) $name"), sha = sha, kind = kind,
        blobId = blobId, name = HistoryCrypto.seal(this@SyncService, name), sizeBytes = size, mime = mime, enc = enc,
    )

    private fun startConnectLoop() {
        scope.launch {
            var backoff = 1000L
            var attempt = 0
            while (isActive && SyncState.running.value) {
                try {
                    val url = settings.serverUrl
                    val pin = settings.spkiPin
                    val token = secrets.token
                    val devId = settings.deviceId
                    if (url == null || pin == null || token == null || devId == null) {
                        // Credentials vanished while running: stop the service so
                        // onDestroy → releaseLocks() runs. A plain `break` would leave
                        // the foreground service alive holding the CPU wakelock +
                        // high-perf Wi-Fi lock forever with zero networking work.
                        update("not paired")
                        SyncState.running.value = false
                        stopSelf()
                        break
                    }
                    attempt++
                    // Snapshot the interface-bound network for this attempt + the
                    // generation we build against, so the inner loops can detect a
                    // mid-session network flip and rebuild promptly.
                    val net = boundNetwork
                    val genAtBuild = netGeneration.get()
                    // Reuse one pinned client across reconnects; rebuild only when the
                    // pin (re-pair) OR the bound network changes. The socketFactory is
                    // baked into the client at build time, so a new Network needs a new
                    // client to pin dials to the fresh interface. Shut the old clients'
                    // executor/pool down before replacing to avoid a leak.
                    if (httpClientPin != pin || httpClientNetwork != net) {
                        listOfNotNull(httpClient, httpPullClient).forEach { old ->
                            runCatching { old.dispatcher.executorService.shutdown(); old.connectionPool.evictAll() }
                        }
                        val sf = net?.socketFactory
                        httpClient = pinnedClient(pin, socketFactory = sf)
                        // Long-timeout client for inbound on-demand preview pulls
                        // (the server broker-pulls from the origin, up to ~60s).
                        httpPullClient = pinnedClient(pin, java.time.Duration.ofSeconds(120), socketFactory = sf)
                        httpClientPin = pin
                        httpClientNetwork = net
                        DebugLog.v("ws") { "pinned client (re)built (net=${if (net != null) "wifi-bound" else "unbound"})" }
                    }
                    val client = httpClient!!
                    val w = WsClient(client)
                    ws = w
                    blob = BlobClient(client, url, token)
                    blobPull = BlobClient(httpPullClient!!, url, token)
                    val incomingJob = launch {
                        // Guard each frame: one throwing/unparseable frame must not kill
                        // the whole receive loop (it'd silently stop all inbound sync).
                        w.incoming.collect { env ->
                            runCatching { handle(env) }
                                .onFailure { DebugLog.e("ws", "수신 프레임 처리 오류(${env.t})", it) }
                        }
                    }
                    // try/finally so the collector coroutine + socket are torn down on
                    // EVERY exit path. Without it, an exception after the launch (e.g.
                    // w.connect or the URL rewrite throwing) would jump to the outer
                    // catch and loop again, orphaning the incomingJob collector (a hot
                    // SharedFlow that never completes) → coroutine/WsClient leak.
                    try {
                        val wsUrl = url.replaceFirst("https://", "wss://")
                            .replaceFirst("http://", "ws://").trimEnd('/') + "/ws"
                        DebugLog.v("ws") { "connect attempt #$attempt → $wsUrl (net=${networkType()}, backoff=${backoff}ms)" }
                        update("connecting…")
                        w.connect(wsUrl, object : WsClient.Listener {
                            override fun onOpen() {
                                SyncState.connected.value = true
                                DebugLog.v("ws") { "onOpen → sending hello (dev=$devId, net=${networkType()})" }
                                w.sendHello(
                                    Hello(deviceId = devId, deviceName = settings.deviceName ?: "android", token = token),
                                )
                                update("connected")
                            }

                            override fun onClosed(reason: String) {
                                SyncState.connected.value = false
                                DebugLog.w("연결 끊김: $reason · 자동 재연결 시도")
                                update("disconnected: $reason")
                            }
                        })

                        var waited = 0
                        while (isActive && !w.connected.value && waited < 8000 && netGeneration.get() == genAtBuild) {
                            delay(200); waited += 200
                        }
                        if (w.connected.value) { backoff = 1000L; DebugLog.v("ws") { "connected; backoff reset to 1000ms" } }
                        else DebugLog.v("ws") { "connect attempt #$attempt did not open within ${waited}ms" }
                        // Also break the hold loop on a network flip so we rebuild the
                        // pinned client on the new interface without waiting a full cycle.
                        while (isActive && SyncState.running.value && w.connected.value && netGeneration.get() == genAtBuild) {
                            delay(1000)
                        }
                    } finally {
                        SyncState.connected.value = false
                        DebugLog.v("ws") { "session ended (running=${SyncState.running.value}); tearing down socket" }
                        incomingJob.cancel()
                        w.close()
                        blob = null
                        blobPull = null
                    }
                } catch (e: Exception) {
                    // SAFE FIX: a CancellationException means the scope is being torn
                    // down (service stopping). Rethrow it so teardown is not mislabeled
                    // as a connection error and the loop unwinds cleanly.
                    if (e is CancellationException) {
                        DebugLog.v("ws") { "connect loop cancelled (teardown)" }
                        throw e
                    }
                    DebugLog.e("ws", "연결/실행 오류 (attempt #$attempt, net=${networkType()})", e)
                    update("error: ${e.message ?: e.javaClass.simpleName}")
                    SyncState.connected.value = false
                }
                if (!SyncState.running.value) break
                DebugLog.v("ws") { "reconnect in ${backoff}ms (next attempt #${attempt + 1})" }
                delay(backoff)
                backoff = (backoff * 2).coerceAtMost(30_000L)
            }
        }
    }

    private suspend fun handle(env: Envelope) {
        when (env.t) {
            MsgType.HELLO_OK -> {
                val ok = runCatching { env.decodePayload<HelloOk>() }
                    .onFailure { DebugLog.w("HELLO_OK 디코딩 실패: ${it.message}") }.getOrNull()
                threshold = ok?.onDemandThreshold ?: 0
                settings.onDemandThreshold = threshold
                SyncState.roster.value = ok?.roster ?: emptyList()
                SyncState.pools.value = ok?.pools?.ifEmpty { listOf("default") } ?: listOf("default")
                SyncState.currentPool.value = ok?.pool?.ifEmpty { "default" } ?: "default"
                update("synced with ${ok?.serverName ?: "server"}")
                refreshNotification()
                // P1 — hello_ok means the session is fully usable (threshold known,
                // blob channel live): drain any clips queued while disconnected/failed.
                flushOutbound()
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
            MsgType.TOKEN_ROTATE -> {
                val tr = runCatching { env.decodePayload<TokenRotate>() }.getOrNull()
                if (tr != null && tr.token.isNotEmpty()) {
                    secrets.token = tr.token
                    DebugLog.i("bearer token rotated + saved")
                }
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
                val ev = runCatching { env.decodePayload<ClipEvent>() }
                    .onFailure { DebugLog.w("받은 클립 디코딩 실패: ${it.message}") }.getOrNull() ?: return
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
                            if (dec != null) text = dec else { text = "[encrypted — no matching key]"; readable = false; DebugLog.w("받은 텍스트 복호화 실패 — 일치하는 E2E 키 없음 (keyId=${ev.enc!!.keyId})") }
                        }
                        dao.insert(
                            ClipEntity(
                                clipId = ev.id, ts = System.currentTimeMillis(), direction = "in",
                                origin = ev.originDeviceId, text = HistoryCrypto.seal(this@SyncService, text),
                                sha = ev.sha256.ifEmpty { sha256Hex(text) }, enc = ev.enc != null,
                            ),
                        )
                        if (Privacy.classify(text) != null) {
                            // Received password-like clip: purge from history after a short TTL.
                            scope.launch { delay(45_000L); runCatching { dao.deleteByClipId(ev.id) } }
                        }
                        if (readable) {
                            var html: String? = ev.html?.takeIf { it.isNotEmpty() }
                            if (html != null && ev.enc != null) {
                                val k = key
                                html = if (k != null) runCatching { String(E2eCrypto.open(k, Base64.decode(html, Base64.NO_WRAP))) }.getOrNull() else null
                            }
                            DebugLog.v("apply") { "applying inbound text to clipboard (${text.toByteArray().size} bytes, html=${html != null})" }
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
                        // On-demand previews trigger a server broker-pull from the
                        // origin (up to ~60s), so use the long-timeout pull client;
                        // eager bytes are already uploaded and use the snappy client.
                        val fetcher = if (ev.onDemand) blobPull else blob
                        var data = runCatching { fetcher?.get(ev.blobId!!) }.getOrNull()
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
                                Notifications.notifyClip(this, ev.originDeviceId, "🖼 $nm", data)
                            } else {
                                // on-demand: thumbnail is cached, but let the user save it explicitly
                                dao.insert(fileEntity(ev.id, "in", ev.originDeviceId, ev.sha256, "image", ev.blobId!!, nm, ev.size, imageMime, enc = ev.enc != null))
                                Notifications.notifyDownloadable(this, ev.originDeviceId, ev.blobId!!, nm, imageMime, ev.enc != null, data)
                            }
                            SyncState.lastEvent.value = "↓ image ${data!!.size / 1024}KB"
                            DebugLog.i("received image ${data!!.size} bytes (onDemand=${ev.onDemand}, e2e=${ev.enc != null})")
                        } else {
                            dao.insert(fileEntity(ev.id, "in", ev.originDeviceId, ev.sha256, "image", ev.blobId!!, nm, ev.size, imageMime, enc = ev.enc != null))
                            Notifications.notifyDownloadable(this, ev.originDeviceId, ev.blobId!!, nm, imageMime, ev.enc != null)
                            DebugLog.w("inbound image: fetch/decrypt failed for ${ev.blobId}")
                        }
                    }
                    ev.blobId != null && !ev.onDemand -> {
                        // Eager (≤ threshold) file of ANY type: fetch, decrypt, and put it
                        // straight on the clipboard so it's immediately pasteable.
                        ensureKey()
                        var data = runCatching { blob?.get(ev.blobId!!) }.getOrNull()
                        if (data != null && ev.enc != null) {
                            val k = key
                            data = if (k != null) runCatching { E2eCrypto.open(k, data!!) }.getOrNull() else null
                        }
                        val nm = ev.name ?: "file"
                        val kind = if (imageMime != null) "image" else "file"
                        if (data != null) {
                            runCatching { File(File(cacheDir, "clip-src").apply { mkdirs() }, ev.blobId!!.removePrefix("sha256:")).writeBytes(data!!) }
                            capture?.applyInboundFile(data!!, nm, settings.sensitiveMark)
                            scheduleAutoClear(sha256Hex(data!!))
                            dao.insert(fileEntity(ev.id, "in", ev.originDeviceId, ev.sha256.ifEmpty { sha256Hex(data!!) }, kind, ev.blobId!!, nm, data!!.size.toLong(), ev.mime.firstOrNull() ?: "", enc = ev.enc != null))
                            Notifications.notifyClip(this, ev.originDeviceId, "📎 $nm", if (imageMime != null) data else null)
                            SyncState.lastEvent.value = "↓ $nm → 클립보드"
                            DebugLog.i("received file → clipboard: $nm (${data!!.size} bytes, e2e=${ev.enc != null})")
                        } else {
                            dao.insert(fileEntity(ev.id, "in", ev.originDeviceId, ev.sha256, kind, ev.blobId!!, nm, ev.size, ev.mime.firstOrNull() ?: "", enc = ev.enc != null))
                            Notifications.notifyDownloadable(this, ev.originDeviceId, ev.blobId!!, nm, ev.mime.firstOrNull() ?: "*/*", ev.enc != null)
                            DebugLog.w("eager file fetch failed → downloadable: $nm")
                        }
                    }
                    ev.blobId != null -> {
                        // On-demand (> threshold): record as downloadable; fetched on tap.
                        val nm = ev.name ?: "file"
                        val kind = if (imageMime != null) "image" else "file"
                        dao.insert(fileEntity(ev.id, "in", ev.originDeviceId, ev.sha256, kind, ev.blobId!!, nm, ev.size, ev.mime.firstOrNull() ?: "", enc = ev.enc != null))
                        Notifications.notifyDownloadable(this, ev.originDeviceId, ev.blobId!!, nm, ev.mime.firstOrNull() ?: "*/*", ev.enc != null)
                        SyncState.lastEvent.value = "↓ $nm (download)"
                        DebugLog.i("received file metadata: $nm (${ev.size} bytes, onDemand=true, e2e=${ev.enc != null})")
                    }
                    else -> DebugLog.i("ignoring unsupported clip (mime=${ev.mime})")
                }
                dao.prune(300)
                refreshNotification()
            }
            MsgType.HELLO_ERR, MsgType.ERROR -> {
                val he = runCatching { env.decodePayload<HelloErr>() }.getOrNull()
                val code = he?.code ?: "error"
                val message = he?.message ?: ""
                DebugLog.w("server rejected: $code${if (message.isNotEmpty()) " — $message" else ""}")
                update("rejected: $code${if (message.isNotEmpty()) " — $message" else ""}")
                // Credential/auth rejections won't recover by retrying the same
                // dead token — stop the reconnect loop and surface a re-pair CTA
                // instead of spinning forever re-sending it.
                if (code == "unauthorized" || code == "bad_hello") {
                    SyncState.needsRepair.value = true
                    SyncState.running.value = false
                    DebugLog.w("auth rejected ($code) — re-pair required; stopping reconnect loop")
                }
            }
            else -> DebugLog.w("unhandled frame type: ${env.t}")
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
        DebugLog.v("svc") { "onDestroy" }
        SyncState.running.value = false
        SyncState.connected.value = false
        capture?.stop()
        capture = null
        ws?.close()
        // Release the ConnectivityManager NetworkCallback (P0).
        runCatching { networkCallback?.let { connectivityMgr?.unregisterNetworkCallback(it) } }
            .onFailure { DebugLog.w("unregisterNetworkCallback failed: ${it.message}") }
        networkCallback = null
        connectivityMgr = null
        boundNetwork = null
        // Release the reused OkHttpClients' Dispatcher executors + ConnectionPools.
        listOfNotNull(httpClient, httpPullClient).forEach { c ->
            runCatching { c.dispatcher.executorService.shutdown(); c.connectionPool.evictAll() }
        }
        httpClient = null
        httpPullClient = null
        httpClientPin = null
        releaseLocks()
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
