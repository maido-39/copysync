package com.copysync.android.net

import kotlinx.serialization.decodeFromString
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.decodeFromJsonElement
import kotlinx.serialization.json.encodeToJsonElement

/** Shared JSON config: tolerant of unknown keys, omits nulls, encodes defaults. */
val json: Json = Json {
    ignoreUnknownKeys = true
    encodeDefaults = true
    explicitNulls = false
}

/** Wrap a typed payload into an envelope frame string. */
inline fun <reified T> encodeEnvelope(type: String, payload: T): String {
    val d = json.encodeToJsonElement(payload)
    return json.encodeToString(Envelope(type, d))
}

/** Decode an envelope's payload into T. */
inline fun <reified T> Envelope.decodePayload(): T = json.decodeFromJsonElement(d)

fun decodeEnvelope(text: String): Envelope? =
    runCatching { json.decodeFromString<Envelope>(text) }.getOrNull()
