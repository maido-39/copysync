package com.copysync.android.sync

import kotlinx.coroutines.flow.MutableStateFlow

/** Process-wide observable sync state, shared between the service and the UI. */
object SyncState {
    val running = MutableStateFlow(false)
    val connected = MutableStateFlow(false)
    val status = MutableStateFlow("stopped")
    val lastEvent = MutableStateFlow("")
}
