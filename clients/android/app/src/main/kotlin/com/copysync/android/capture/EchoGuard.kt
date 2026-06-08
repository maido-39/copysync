package com.copysync.android.capture

import android.os.SystemClock

/**
 * Tracks the SHA-256 of clipboard items we just produced (whether by writing an
 * inbound clip or by emitting a captured one), so the resulting change events
 * don't loop back. A single recent-set covers both the write-echo and the
 * duplicate-trigger (listener + logcat firing for the same change) cases.
 */
class EchoGuard(private val ttlMs: Long = 4000L) {
    private val seen = HashMap<String, Long>()

    @Synchronized
    fun mark(sha: String) {
        prune()
        seen[sha] = SystemClock.elapsedRealtime()
    }

    @Synchronized
    fun seenRecently(sha: String): Boolean {
        prune()
        return seen.containsKey(sha)
    }

    private fun prune() {
        val cutoff = SystemClock.elapsedRealtime() - ttlMs
        val it = seen.entries.iterator()
        while (it.hasNext()) {
            if (it.next().value < cutoff) it.remove()
        }
    }
}
