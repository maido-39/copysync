package com.copysync.android.net

import java.security.MessageDigest

fun sha256Hex(bytes: ByteArray): String =
    MessageDigest.getInstance("SHA-256").digest(bytes).joinToString("") { "%02x".format(it) }

fun sha256Hex(text: String): String = sha256Hex(text.toByteArray(Charsets.UTF_8))
