package com.copysync.android.net

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonPrimitive

/** Protocol version this client speaks (see docs/PROTOCOL.md). */
const val PROTO = 1

object MsgType {
    const val HELLO = "hello"
    const val HELLO_OK = "hello_ok"
    const val HELLO_ERR = "hello_err"
    const val CLIP = "clip"
    const val ACK = "ack"
    const val BLOB_REQUEST = "blob_request"
    const val PRESENCE = "presence"
    const val ROSTER = "roster"
    const val ERROR = "error"
}

/** Every control-channel frame: {"t": type, "d": payload}. */
@Serializable
data class Envelope(val t: String, val d: JsonElement)

@Serializable
data class Hello(
    val deviceId: String,
    val deviceName: String,
    val token: String,
    val platform: String = "android",
    val proto: Int = PROTO,
)

@Serializable
data class DeviceInfo(
    val id: String,
    val name: String = "",
    val platform: String = "",
    val online: Boolean = false,
)

@Serializable
data class HelloOk(
    val serverId: String = "",
    val serverName: String = "",
    val e2e: Boolean = false,
    val maxMsg: Long = 0,
    val blobCap: Long = 0,
    val onDemandThreshold: Long = 0,
    val roster: List<DeviceInfo> = emptyList(),
)

@Serializable
data class BlobRequest(val id: String = "")

@Serializable
data class HelloErr(val code: String = "", val message: String = "")

/**
 * A clipboard item. `targets` is a raw JsonElement so it round-trips the
 * server's "all" | [ids] union without a custom serializer; outbound clips
 * default to broadcasting to all devices.
 */
@Serializable
data class ClipEvent(
    val id: String,
    val originDeviceId: String = "",
    val seq: Long = 0,
    val ts: String? = null, // omitted when unset; server stamps its own receive time
    val mime: List<String> = emptyList(),
    val inlineText: String? = null,
    val html: String? = null, // rich-text (text/html) variant; ciphertext (base64) when E2E
    val blobId: String? = null,
    val name: String? = null,
    val onDemand: Boolean = false,
    val size: Long = 0,
    val sha256: String = "",
    val enc: EncMeta? = null,
    val targets: JsonElement = JsonPrimitive("all"),
)

/** Marks a clip payload as E2E ciphertext. */
@Serializable
data class EncMeta(val alg: String = "", val keyId: String = "", val nonce: String = "")

@Serializable
data class Ack(
    val id: String,
    val status: String = "",
    val queuedFor: List<String> = emptyList(),
)

@Serializable
data class Presence(val device: DeviceInfo, val online: Boolean)

@Serializable
data class Roster(val devices: List<DeviceInfo> = emptyList())
