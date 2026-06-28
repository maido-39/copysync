package com.copysync.android.net

import com.copysync.android.sync.DebugLog
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener

/** A thin wrapper over OkHttp's WebSocket exposing decoded frames as a flow. */
class WsClient(private val client: OkHttpClient) {

    interface Listener {
        fun onOpen()
        fun onClosed(reason: String)
    }

    private var ws: WebSocket? = null
    val incoming = MutableSharedFlow<Envelope>(extraBufferCapacity = 128)
    val connected = MutableStateFlow(false)

    fun connect(wsUrl: String, listener: Listener) {
        DebugLog.v("ws") { "connect start → $wsUrl" }
        val req = Request.Builder().url(wsUrl).build()
        ws = client.newWebSocket(req, object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                connected.value = true
                DebugLog.v("ws") { "connect success (HTTP ${response.code} ${response.message})" }
                listener.onOpen()
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                val env = decodeEnvelope(text)
                when {
                    env == null -> DebugLog.w("프레임 파싱 실패: ${text.take(80)}")
                    !incoming.tryEmit(env) -> DebugLog.w("수신 버퍼 가득 — 프레임 누락(${env.t})")
                    else -> DebugLog.v("ws") { "recv frame t=${env.t} (${text.length} chars)" }
                }
            }

            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                // Complete the WS close handshake immediately so onClosed fires
                // deterministically instead of waiting on the connect loop's ~1s poll.
                // OkHttp does NOT auto-send the responding close frame; the app must.
                // The responding code must be 1000 (validateCloseCode rejects reserved
                // codes like 1001), but we still report the server's ACTUAL code +
                // reason verbatim in the log/listener so the real cause stays visible.
                DebugLog.v("ws") { "disconnect (onClosing) — server code=$code reason='${reason}' (server-initiated)" }
                webSocket.close(1000, null)
                connected.value = false
                listener.onClosed(if (reason.isNotEmpty()) "$reason (code $code)" else "code $code")
            }

            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                connected.value = false
                DebugLog.v("ws") { "disconnect (onClosed) — code=$code reason='${reason}'" }
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                connected.value = false
                // Include the exception type + any HTTP status so pin/token/handshake
                // failures are distinguishable in the Debug tab.
                val http = response?.let { " (HTTP ${it.code})" } ?: ""
                DebugLog.e("ws", "connect/socket failure$http", t)
                listener.onClosed("${t.javaClass.simpleName}: ${t.message ?: "connection failure"}$http")
            }
        })
    }

    fun sendHello(h: Hello) = sendRaw(encodeEnvelope(MsgType.HELLO, h))
    fun sendClip(c: ClipEvent) = sendRaw(encodeEnvelope(MsgType.CLIP, c))
    fun setPool(pool: String) = sendRaw(encodeEnvelope(MsgType.SET_POOL, SetPool(pool)))

    private fun sendRaw(frame: String): Boolean = ws?.send(frame) ?: false

    fun close() {
        DebugLog.v("ws") { "disconnect (close) — client-initiated, sending code 1000" }
        ws?.close(1000, null)
        ws = null
        connected.value = false
    }
}
