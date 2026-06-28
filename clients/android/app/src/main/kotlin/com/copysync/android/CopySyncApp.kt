package com.copysync.android

import android.app.Application
import com.copysync.android.sync.DebugLog

class CopySyncApp : Application() {
    override fun onCreate() {
        super.onCreate()
        // Wire the persistent rotating-file log sink + restore the verbose pref,
        // so a crash trail survives a process kill from the very first event.
        DebugLog.init(this)
    }
}
