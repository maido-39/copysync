package com.copysync.android.net

import android.util.Base64
import com.copysync.android.sync.DebugLog
import okhttp3.OkHttpClient
import java.security.MessageDigest
import java.security.SecureRandom
import java.security.cert.CertificateException
import java.security.cert.X509Certificate
import java.time.Duration
import javax.net.SocketFactory
import javax.net.ssl.SSLContext
import javax.net.ssl.X509TrustManager

/**
 * Builds an OkHttpClient that trusts EXACTLY the server whose leaf-certificate
 * SubjectPublicKeyInfo SHA-256 equals the given pin (base64). Trust is anchored
 * on the pin, not a CA — the standard model for a self-signed LAN server.
 *
 * X509Certificate.publicKey.encoded is the SubjectPublicKeyInfo DER, matching
 * the server's sha256(MarshalPKIXPublicKey(pub)).
 *
 * [socketFactory] pins every dial to a specific network interface (the Wi-Fi/LAN
 * Network chosen by SyncService's NetworkCallback). Without it, a Wi-Fi↔cellular
 * switch makes OkHttp redial over cellular against the LAN-only server → the
 * source becomes the clatd 192.0.0.2 464XLAT address and the connect blackholes
 * into a SocketTimeout. Passing network.socketFactory forces the dial to leave
 * the correct interface. Also sets an explicit connectTimeout so a wrong-interface
 * dial fails fast (instead of the default 10s) and the reconnect loop retries.
 */
fun pinnedClient(
    spkiPinB64: String,
    readTimeout: Duration = Duration.ZERO,
    socketFactory: SocketFactory? = null,
): OkHttpClient {
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
        .connectTimeout(Duration.ofSeconds(8))
    if (socketFactory != null) {
        // Pin the underlying TCP socket to the chosen (Wi-Fi/LAN) Network. OkHttp
        // wraps the plain socket from this factory with the SSLSocketFactory above,
        // so pinning + interface-binding compose correctly.
        b.socketFactory(socketFactory)
    }
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
