package com.copysync.android.data

import android.content.Context
import android.util.Base64
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/**
 * At-rest encryption for the local history's text/name fields — mirrors the
 * desktop client. A random 32-byte key (kept in Keystore-backed
 * EncryptedSharedPreferences via [Secrets]) seals each field with AES-256-GCM.
 *
 * Sealed values carry an "enc1:" prefix; rows written before this feature (no
 * prefix) are read back as-is, so existing history keeps working.
 */
object HistoryCrypto {
    private const val PREFIX = "enc1:"
    private const val IV_LEN = 12
    private const val TAG_BITS = 128

    // Cached so we don't rebuild EncryptedSharedPreferences (a Keystore op) on
    // every field — open() runs hundreds of times when the history list renders.
    @Volatile
    private var cached: SecretKeySpec? = null

    private fun key(ctx: Context): SecretKeySpec {
        cached?.let { return it }
        return synchronized(this) {
            cached ?: run {
                val s = Secrets(ctx)
                val k = s.historyKey ?: Base64.encodeToString(
                    ByteArray(32).also { SecureRandom().nextBytes(it) }, Base64.NO_WRAP,
                ).also { s.historyKey = it }
                SecretKeySpec(Base64.decode(k, Base64.NO_WRAP), "AES").also { cached = it }
            }
        }
    }

    /** Encrypt a field for storage. Empty stays empty; on any failure the
     *  plaintext is returned so a clip is never lost to a crypto error. */
    fun seal(ctx: Context, plain: String): String {
        if (plain.isEmpty()) return plain
        return runCatching {
            val iv = ByteArray(IV_LEN).also { SecureRandom().nextBytes(it) }
            val c = Cipher.getInstance("AES/GCM/NoPadding")
            c.init(Cipher.ENCRYPT_MODE, key(ctx), GCMParameterSpec(TAG_BITS, iv))
            val ct = c.doFinal(plain.toByteArray(Charsets.UTF_8))
            PREFIX + Base64.encodeToString(iv + ct, Base64.NO_WRAP)
        }.getOrDefault(plain)
    }

    /** Decrypt a stored field. Legacy plaintext (no prefix) is returned as-is. */
    fun open(ctx: Context, stored: String): String {
        if (!stored.startsWith(PREFIX)) return stored
        return runCatching {
            val blob = Base64.decode(stored.removePrefix(PREFIX), Base64.NO_WRAP)
            val iv = blob.copyOfRange(0, IV_LEN)
            val ct = blob.copyOfRange(IV_LEN, blob.size)
            val c = Cipher.getInstance("AES/GCM/NoPadding")
            c.init(Cipher.DECRYPT_MODE, key(ctx), GCMParameterSpec(TAG_BITS, iv))
            String(c.doFinal(ct), Charsets.UTF_8)
        }.getOrDefault("")
    }
}
