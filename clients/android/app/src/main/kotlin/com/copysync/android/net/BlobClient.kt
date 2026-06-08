package com.copysync.android.net

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.MediaType.Companion.toMediaTypeOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.asRequestBody
import okhttp3.RequestBody.Companion.toRequestBody
import java.io.File

/** Uploads/downloads large payloads over the content-addressed blob channel. */
class BlobClient(
    private val client: OkHttpClient,
    serverUrl: String,
    private val token: String,
) {
    private val base = serverUrl.trimEnd('/')

    /** Upload content, returning its "sha256:<hex>" id. Idempotent server-side. */
    fun put(content: ByteArray): String {
        val id = "sha256:" + sha256Hex(content)
        val req = Request.Builder()
            .url("$base/blob/$id")
            .header("Authorization", "Bearer $token")
            .put(content.toRequestBody(OCTET))
            .build()
        client.newCall(req).execute().use { resp ->
            require(resp.isSuccessful) { "blob PUT ${resp.code}" }
        }
        return id
    }

    /** Stream a file to the blob channel under a known id (the file's sha256). */
    fun putFile(file: File, contentType: String, id: String): String {
        val mt = contentType.ifEmpty { "application/octet-stream" }.toMediaTypeOrNull()
        val req = Request.Builder()
            .url("$base/blob/$id")
            .header("Authorization", "Bearer $token")
            .put(file.asRequestBody(mt))
            .build()
        client.newCall(req).execute().use { resp ->
            require(resp.isSuccessful) { "blob PUT ${resp.code}" }
        }
        return id
    }

    fun get(id: String): ByteArray {
        val req = Request.Builder()
            .url("$base/blob/$id")
            .header("Authorization", "Bearer $token")
            .get()
            .build()
        client.newCall(req).execute().use { resp ->
            require(resp.isSuccessful) { "blob GET ${resp.code}" }
            return resp.body?.bytes() ?: ByteArray(0)
        }
    }

    companion object {
        private val OCTET = "application/octet-stream".toMediaType()
    }
}
