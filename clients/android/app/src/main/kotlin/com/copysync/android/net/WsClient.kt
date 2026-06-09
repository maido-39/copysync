package com.copysync.android.net

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
        val req = Request.Builder().url(wsUrl).build()
        ws = client.newWebSocket(req, object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                connected.value = true
                listener.onOpen()
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                decodeEnvelope(text)?.let { incoming.tryEmit(it) }
            }

            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                connected.value = false
                webSocket.close(1000, null)
                listener.onClosed(reason)
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                connected.value = false
                listener.onClosed(t.message ?: "connection failure")
            }
        })
    }

    fun sendHello(h: Hello) = sendRaw(encodeEnvelope(MsgType.HELLO, h))
    fun sendClip(c: ClipEvent) = sendRaw(encodeEnvelope(MsgType.CLIP, c))
    fun setPool(pool: String) = sendRaw(encodeEnvelope(MsgType.SET_POOL, SetPool(pool)))

    private fun sendRaw(frame: String): Boolean = ws?.send(frame) ?: false

    fun close() {
        ws?.close(1000, null)
        ws = null
        connected.value = false
    }
}
