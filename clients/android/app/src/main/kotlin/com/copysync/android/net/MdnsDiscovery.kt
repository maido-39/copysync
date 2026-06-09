package com.copysync.android.net

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Handler
import android.os.Looper

/**
 * Discovers CopySync servers advertised over mDNS (_copysync._tcp) on the local
 * link. Results (name → https URL) are delivered on the main thread. Note: mDNS
 * is link-local — it does not cross a VPN/WireGuard tunnel or VLAN, so a remote
 * server still needs its IP entered manually.
 */
@Suppress("DEPRECATION") // NsdManager.resolveService is deprecated (API 34) but works across our min/target.
class MdnsDiscovery(ctx: Context) {
    private val nsd = ctx.getSystemService(NsdManager::class.java)
    private val main = Handler(Looper.getMainLooper())
    private var listener: NsdManager.DiscoveryListener? = null

    fun start(onFound: (name: String, url: String) -> Unit) {
        stop()
        val l = object : NsdManager.DiscoveryListener {
            override fun onStartDiscoveryFailed(s: String, e: Int) {}
            override fun onStopDiscoveryFailed(s: String, e: Int) {}
            override fun onDiscoveryStarted(s: String) {}
            override fun onDiscoveryStopped(s: String) {}
            override fun onServiceLost(info: NsdServiceInfo) {}
            override fun onServiceFound(info: NsdServiceInfo) {
                runCatching {
                    nsd.resolveService(info, object : NsdManager.ResolveListener {
                        override fun onResolveFailed(i: NsdServiceInfo, e: Int) {}
                        override fun onServiceResolved(i: NsdServiceInfo) {
                            val host = i.host?.hostAddress ?: return
                            main.post { onFound(i.serviceName, "https://$host:${i.port}") }
                        }
                    })
                }
            }
        }
        listener = l
        runCatching { nsd.discoverServices("_copysync._tcp", NsdManager.PROTOCOL_DNS_SD, l) }
    }

    fun stop() {
        listener?.let { runCatching { nsd.stopServiceDiscovery(it) } }
        listener = null
    }
}
