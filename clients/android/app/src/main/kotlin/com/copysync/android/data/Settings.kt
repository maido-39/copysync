package com.copysync.android.data

import android.content.Context

/** Non-secret paired-server configuration, persisted in SharedPreferences. */
class Settings(context: Context) {
    private val prefs = context.getSharedPreferences("copysync", Context.MODE_PRIVATE)

    var serverUrl: String?
        get() = prefs.getString(KEY_URL, null)
        set(v) = prefs.edit().putString(KEY_URL, v).apply()

    var serverName: String?
        get() = prefs.getString(KEY_SRV_NAME, null)
        set(v) = prefs.edit().putString(KEY_SRV_NAME, v).apply()

    var serverId: String?
        get() = prefs.getString(KEY_SRV_ID, null)
        set(v) = prefs.edit().putString(KEY_SRV_ID, v).apply()

    var deviceId: String?
        get() = prefs.getString(KEY_DEV_ID, null)
        set(v) = prefs.edit().putString(KEY_DEV_ID, v).apply()

    var deviceName: String?
        get() = prefs.getString(KEY_DEV_NAME, null)
        set(v) = prefs.edit().putString(KEY_DEV_NAME, v).apply()

    var spkiPin: String?
        get() = prefs.getString(KEY_PIN, null)
        set(v) = prefs.edit().putString(KEY_PIN, v).apply()

    var e2e: Boolean
        get() = prefs.getBoolean(KEY_E2E, false)
        set(v) = prefs.edit().putBoolean(KEY_E2E, v).apply()

    var onDemandThreshold: Long
        get() = prefs.getLong(KEY_THRESH, 0)
        set(v) = prefs.edit().putLong(KEY_THRESH, v).apply()

    var sensitiveMark: Boolean
        get() = prefs.getBoolean(KEY_SENS, false)
        set(v) = prefs.edit().putBoolean(KEY_SENS, v).apply()

    /** Privacy filter: don't sync clips classified sensitive (passwords, cards…). */
    var excludeSensitive: Boolean
        get() = prefs.getBoolean(KEY_EXCLUDE, true)
        set(v) = prefs.edit().putBoolean(KEY_EXCLUDE, v).apply()

    var autoClearSeconds: Int
        get() = prefs.getInt(KEY_AUTOCLEAR, 0)
        set(v) = prefs.edit().putInt(KEY_AUTOCLEAR, v).apply()

    val isPaired: Boolean
        get() = !serverUrl.isNullOrEmpty() && !deviceId.isNullOrEmpty() && !spkiPin.isNullOrEmpty()

    fun clear() = prefs.edit().clear().apply()

    private companion object {
        const val KEY_URL = "serverUrl"
        const val KEY_SRV_NAME = "serverName"
        const val KEY_SRV_ID = "serverId"
        const val KEY_DEV_ID = "deviceId"
        const val KEY_DEV_NAME = "deviceName"
        const val KEY_PIN = "spkiPin"
        const val KEY_E2E = "e2e"
        const val KEY_THRESH = "onDemandThreshold"
        const val KEY_SENS = "sensitiveMark"
        const val KEY_EXCLUDE = "excludeSensitive"
        const val KEY_AUTOCLEAR = "autoClearSeconds"
    }
}
