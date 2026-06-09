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

    /** Trust-on-first-use: fetch the server's identity + pin without verification. */
    fun fetchServerInfo(serverUrl: String): ServerInfo {
        val req = Request.Builder().url(serverUrl.trimEnd('/') + "/pair/serverinfo").get().build()
        insecureClient().newCall(req).execute().use { resp ->
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
    val pin = pinOpt.ifBlank { PairingClient.fetchServerInfo(serverUrl).spkiPin }
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
