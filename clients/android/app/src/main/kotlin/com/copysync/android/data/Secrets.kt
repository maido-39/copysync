package com.copysync.android.data

import android.content.Context
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey

/** The device bearer token, stored encrypted (AES-256, Keystore-backed master key). */
class Secrets(context: Context) {
    private val prefs by lazy {
        val masterKey = MasterKey.Builder(context)
            .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
            .build()
        EncryptedSharedPreferences.create(
            context,
            "copysync_secrets",
            masterKey,
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
        )
    }

    var token: String?
        get() = prefs.getString("token", null)
        set(v) = prefs.edit().putString("token", v).apply()

    /** Optional E2E passphrase (empty/null = E2E off). */
    var e2ePass: String?
        get() = prefs.getString("e2ePass", null)
        set(v) = prefs.edit().putString("e2ePass", v).apply()

    fun clear() = prefs.edit().clear().apply()
}
