package com.copysync.android.net

import android.util.Base64
import com.copysync.android.sync.DebugLog
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
fun pinnedClient(spkiPinB64: String, readTimeout: Duration = Duration.ZERO): OkHttpClient {
    val pin = Base64.decode(spkiPinB64, Base64.DEFAULT)
    val tm = object : X509TrustManager {
        override fun checkClientTrusted(chain: Array<out X509Certificate>?, authType: String?) {}
        override fun checkServerTrusted(chain: Array<out X509Certificate>?, authType: String?) {
            val leaf = chain?.firstOrNull()
                ?: throw CertificateException("no server certificate").also {
                    DebugLog.e("pin", "TLS pin check: no server certificate in chain", it)
                }
            val spki = MessageDigest.getInstance("SHA-256").digest(leaf.publicKey.encoded)
            if (!spki.contentEquals(pin)) {
                val ex = CertificateException("server SPKI pin mismatch (possible MITM)")
                DebugLog.e(
                    "pin",
                    "TLS pin MISMATCH (possible MITM): presented=${Base64.encodeToString(spki, Base64.NO_WRAP)}",
                    ex,
                )
                throw ex
            }
            DebugLog.v("pin") { "TLS pin OK (authType=$authType)" }
        }
        override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
    }
    val ssl = SSLContext.getInstance("TLS").apply { init(null, arrayOf(tm), SecureRandom()) }
    val b = OkHttpClient.Builder()
        .sslSocketFactory(ssl.socketFactory, tm)
        .hostnameVerifier { _, _ -> true } // SPKI pin is the real check
        .pingInterval(Duration.ofSeconds(20))
    if (!readTimeout.isZero) {
        // On-demand GETs long-poll while the server pulls from the origin device.
        b.readTimeout(readTimeout).callTimeout(readTimeout.plusSeconds(15))
    }
    return b.build()
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
