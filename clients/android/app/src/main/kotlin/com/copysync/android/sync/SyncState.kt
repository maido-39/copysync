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
}
