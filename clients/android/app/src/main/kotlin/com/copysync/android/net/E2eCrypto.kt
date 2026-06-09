package com.copysync.android.net

import org.bouncycastle.crypto.generators.Argon2BytesGenerator
import org.bouncycastle.crypto.params.Argon2Parameters
import java.security.MessageDigest
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/**
 * Client-side end-to-end crypto, byte-compatible with the Go reference client:
 * Argon2id(passphrase, salt = sha256("copysync-e2e|"+serverId)) → 32-byte key,
 * then AES-256-GCM with a 12-byte nonce prepended to the ciphertext. The server
 * never sees the passphrase or the plaintext.
 */
object E2eCrypto {
    private const val ALG = "aes-256-gcm"

    fun deriveKey(pass: String, serverId: String): ByteArray {
        val salt = MessageDigest.getInstance("SHA-256").digest("copysync-e2e|$serverId".toByteArray(Charsets.UTF_8))
        val params = Argon2Parameters.Builder(Argon2Parameters.ARGON2_id)
            .withVersion(Argon2Parameters.ARGON2_VERSION_13)
            .withIterations(1)
            .withMemoryAsKB(64 * 1024)
            .withParallelism(4)
            .withSalt(salt)
            .build()
        val gen = Argon2BytesGenerator().apply { init(params) }
        val out = ByteArray(32)
        gen.generateBytes(pass.toByteArray(Charsets.UTF_8), out)
        return out
    }

    fun keyId(key: ByteArray): String = sha256Hex(key).substring(0, 16)

    /** nonce(12) ‖ ciphertext‖tag */
    fun seal(key: ByteArray, plaintext: ByteArray): ByteArray {
        val nonce = ByteArray(12).also { SecureRandom().nextBytes(it) }
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(128, nonce))
        return nonce + c.doFinal(plaintext)
    }

    fun open(key: ByteArray, raw: ByteArray): ByteArray {
        require(raw.size >= 12) { "ciphertext too short" }
        val nonce = raw.copyOfRange(0, 12)
        val ct = raw.copyOfRange(12, raw.size)
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.DECRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(128, nonce))
        return c.doFinal(ct)
    }
}
