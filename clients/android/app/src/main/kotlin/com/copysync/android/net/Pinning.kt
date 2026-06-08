package com.copysync.android.net

import android.util.Base64
import okhttp3.OkHttpClient
import java.security.MessageDigest
import java.security.SecureRandom
import java.security.cert.CertificateException
import java.security.cert.X509Certificate
import java.time.Duration
import javax.net.ssl.SSLContext
import javax.net.ssl.X509TrustManager

/**
 * Builds an OkHttpClient that trusts EXACTLY the server whose leaf-certificate
 * SubjectPublicKeyInfo SHA-256 equals the given pin (base64). Trust is anchored
 * on the pin, not a CA — the standard model for a self-signed LAN server.
 *
 * X509Certificate.publicKey.encoded is the SubjectPublicKeyInfo DER, matching
 * the server's sha256(MarshalPKIXPublicKey(pub)).
 */
fun pinnedClient(spkiPinB64: String): OkHttpClient {
    val pin = Base64.decode(spkiPinB64, Base64.DEFAULT)
    val tm = object : X509TrustManager {
        override fun checkClientTrusted(chain: Array<out X509Certificate>?, authType: String?) {}
        override fun checkServerTrusted(chain: Array<out X509Certificate>?, authType: String?) {
            val leaf = chain?.firstOrNull() ?: throw CertificateException("no server certificate")
            val spki = MessageDigest.getInstance("SHA-256").digest(leaf.publicKey.encoded)
            if (!spki.contentEquals(pin)) throw CertificateException("server SPKI pin mismatch (possible MITM)")
        }
        override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
    }
    val ssl = SSLContext.getInstance("TLS").apply { init(null, arrayOf(tm), SecureRandom()) }
    return OkHttpClient.Builder()
        .sslSocketFactory(ssl.socketFactory, tm)
        .hostnameVerifier { _, _ -> true } // SPKI pin is the real check
        .pingInterval(Duration.ofSeconds(20))
        .build()
}

/** An OkHttpClient that skips all TLS verification — used ONLY for the
 *  trust-on-first-use pin discovery during pairing, never for data. */
fun insecureClient(): OkHttpClient {
    val tm = object : X509TrustManager {
        override fun checkClientTrusted(chain: Array<out X509Certificate>?, authType: String?) {}
        override fun checkServerTrusted(chain: Array<out X509Certificate>?, authType: String?) {}
        override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
    }
    val ssl = SSLContext.getInstance("TLS").apply { init(null, arrayOf(tm), SecureRandom()) }
    return OkHttpClient.Builder()
        .sslSocketFactory(ssl.socketFactory, tm)
        .hostnameVerifier { _, _ -> true }
        .build()
}
