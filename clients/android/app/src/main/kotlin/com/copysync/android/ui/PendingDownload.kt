package com.copysync.android.ui

import kotlinx.coroutines.flow.MutableStateFlow

/** A request to download a received blob to a user-chosen location. */
data class DownloadReq(
    val blobId: String,
    val name: String,
    val mime: String,
    val rowid: Long = -1,
    val encrypted: Boolean = false,
)

/** Bridges a notification tap / history button to the in-app save-location picker. */
object PendingDownload {
    val req = MutableStateFlow<DownloadReq?>(null)
}
