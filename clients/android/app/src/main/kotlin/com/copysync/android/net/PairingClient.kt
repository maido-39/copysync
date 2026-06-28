package com.copysync.android.net

import android.content.Context
import com.copysync.android.data.Secrets
import com.copysync.android.data.Settings
import kotlinx.serialization.Serializable
import kotlinx.serialization.decodeFromString
import kotlinx.serialization.encodeToString
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody

@Serializable
data class ServerInfo(
    val serverId: String = "",
    val serverName: String = "",
    val spkiPin: String = "",
    val proto: Int = 0,
)

@Serializable
private data class ClaimRequest(
    val otp: String,
    val deviceName: String,
    val platform: String = "android",
)

@Serializable
data class ClaimResult(
    val deviceId: String = "",
    val token: String = "",
    val serverId: String = "",
    val serverName: String = "",
    val e2e: Boolean = false,
)

/** Pairing QR payload — the JSON the admin UI encodes into the QR code. */
@Serializable
data class PairQr(
    val serverId: String = "",
    val serverName: String = "",
    val host: String = "",
    val port: String = "",
    val spkiPin: String = "",
    val otp: String = "",
)

/** Parse a scanned pairing QR payload. */
fun parsePairQr(s: String): PairQr = json.decodeFromString(s)



/** Handles the HTTP side of device pairing. */
object PairingClient {
    private val JSON = "application/json".toMediaType()

    /**
     * Fetch the server's identity + pin. When [pin] is non-blank the connection
     * is SPKI-pinned to that key (consistent with WsClient/data-plane pinning),
     * so a MITM cannot answer. Only when [pin] is blank does this fall back to an
     * unverified (trust-on-first-use) connection for initial pin discovery.
     */
    fun fetchServerInfo(serverUrl: String, pin: String = ""): ServerInfo {
        val req = Request.Builder().url(serverUrl.trimEnd('/') + "/pair/serverinfo").get().build()
        val client = if (pin.isNotBlank()) pinnedClient(pin) else insecureClient()
        client.newCall(req).execute().use { resp ->
            val body = resp.body?.string().orEmpty()
            require(resp.isSuccessful) { "serverinfo HTTP ${resp.code}" }
            return json.decodeFromString(body)
        }
    }

    /** Redeem an OTP over the pinned connection and return the device credentials. */
    fun claim(client: OkHttpClient, serverUrl: String, otp: String, deviceName: String): ClaimResult {
        val payload = json.encodeToString(ClaimRequest(otp = otp, deviceName = deviceName))
        val req = Request.Builder()
            .url(serverUrl.trimEnd('/') + "/pair/claim")
            .post(payload.toRequestBody(JSON))
            .build()
        client.newCall(req).execute().use { resp ->
            val body = resp.body?.string().orEmpty()
            require(resp.isSuccessful) { "pair failed (HTTP ${resp.code}): $body" }
            return json.decodeFromString(body)
        }
    }
}

/**
 * Pairs and persists credentials in one step: resolves the SPKI pin (trust on
 * first use when [pinOpt] is blank), redeems the OTP over the pinned connection,
 * and stores the server config + token. Blocking; call off the main thread.
 */
fun claimAndStore(ctx: Context, serverUrl: String, otp: String, deviceName: String, pinOpt: String): ClaimResult {
    val pin: String
    if (pinOpt.isNotBlank()) {
        // Out-of-band pin (QR or manual entry): the trusted path.
        pin = pinOpt
    } else {
        // No out-of-band pin: discover it (trust-on-first-use), then BIND the
        // discovered pin to the cert the server actually presents by re-fetching
        // /pair/serverinfo over a connection SPKI-pinned to that pin. This rejects
        // a host that advertises a pin it cannot prove ownership of, and makes the
        // discovered pin consistent with the data-plane (WsClient) pinning that all
        // subsequent traffic relies on. (A fully active on-path MITM can still be
        // self-consistent here — that residual risk is the documented TOFU tradeoff;
        // prefer the QR path to eliminate it.)
        val discovered = PairingClient.fetchServerInfo(serverUrl).spkiPin
        require(discovered.isNotBlank()) { "server did not provide a pin" }
        val verified = runCatching { PairingClient.fetchServerInfo(serverUrl, discovered) }
            .getOrElse { throw IllegalStateException("핀 검증 실패: 서버가 광고한 SPKI 핀과 인증서가 일치하지 않습니다 (MITM 의심)", it) }
        require(verified.spkiPin == discovered) { "서버 핀 불일치 (MITM 의심)" }
        com.copysync.android.sync.DebugLog.w(
            "pairing: trust-on-first-use pin discovered + cert-bound (serverId=${verified.serverId}, pin=${discovered.take(16)}…) — QR 페어링이 더 안전합니다",
        )
        pin = discovered
    }
    require(pin.isNotBlank()) { "server did not provide a pin" }
    val result = PairingClient.claim(pinnedClient(pin), serverUrl, otp, deviceName)
    Settings(ctx).apply {
        this.serverUrl = serverUrl
        serverName = result.serverName.ifBlank { serverUrl }
        serverId = result.serverId
        deviceId = result.deviceId
        this.deviceName = deviceName
        spkiPin = pin
        e2e = result.e2e
    }
    Secrets(ctx).token = result.token
    return result
}
