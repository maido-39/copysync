package com.copysync.android.sync

import com.copysync.android.net.DeviceInfo
import kotlinx.coroutines.flow.MutableStateFlow

/** Process-wide observable sync state, shared between the service and the UI. */
object SyncState {
    val running = MutableStateFlow(false)
    val connected = MutableStateFlow(false)
    val status = MutableStateFlow("stopped")
    val lastEvent = MutableStateFlow("")

    /** Known peer devices (from hello_ok roster + presence deltas), for routing. */
    val roster = MutableStateFlow<List<DeviceInfo>>(emptyList())

    /** Selected routing targets by device id; empty = broadcast to all. */
    val targets = MutableStateFlow<Set<String>>(emptySet())

    /** Share pool: clips only sync among devices in the same pool. */
    val pools = MutableStateFlow<List<String>>(emptyList())
    val currentPool = MutableStateFlow("default")

    /** A clip the privacy filter just blocked from syncing — drives the toast. */
    val blockedToast = MutableStateFlow<BlockedClip?>(null)

    /**
     * Set when the server rejects this device's credentials (hello_err with an
     * auth-type code). The UI/notification layer observes this to surface an
     * actionable "re-pair this device" prompt instead of looping a dead token.
     */
    val needsRepair = MutableStateFlow(false)
}

/** Reason + content preview for a blocked-clip toast. `at` makes each emit distinct. */
data class BlockedClip(val reason: String, val preview: String, val at: Long)
