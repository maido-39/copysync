package com.copysync.android.net

import kotlin.math.ln

/**
 * Client-side sensitivity classifier — mirrors `copysync-core::privacy` (Rust) so
 * Windows and Android agree on what counts as sensitive. Detects passwords, OTP
 * secrets, payment cards, private keys, and user regexes. Pure (JVM-testable).
 */
object Privacy {
    enum class Sensitivity(val label: String) {
        PRIVATE_KEY("private key"),
        OTP_AUTH("OTP secret"),
        CREDIT_CARD("payment card"),
        PASSWORD_LIKE("password-like"),
        CUSTOM("custom pattern"),
    }

    fun classify(text: String, custom: List<Regex> = emptyList()): Sensitivity? {
        val t = text.trim()
        if (t.isEmpty()) return null
        if (t.contains("-----BEGIN") && t.contains("PRIVATE KEY")) return Sensitivity.PRIVATE_KEY
        if (t.length >= 10 && t.substring(0, 10).equals("otpauth://", ignoreCase = true)) return Sensitivity.OTP_AUTH
        if (isCardNumber(t)) return Sensitivity.CREDIT_CARD
        for (re in custom) if (re.containsMatchIn(t)) return Sensitivity.CUSTOM
        if (isPasswordLike(t)) return Sensitivity.PASSWORD_LIKE
        return null
    }

    private fun isCardNumber(t: String): Boolean {
        if (!t.all { it.isDigit() || it == ' ' || it == '-' }) return false
        val digits = t.filter { it.isDigit() }
        if (digits.length !in 13..19) return false
        return luhnOk(digits)
    }

    private fun luhnOk(digits: String): Boolean {
        var sum = 0
        var alt = false
        for (i in digits.indices.reversed()) {
            var v = digits[i] - '0'
            if (alt) {
                v *= 2
                if (v > 9) v -= 9
            }
            sum += v
            alt = !alt
        }
        return sum % 10 == 0
    }

    private fun looksLikeUrl(t: String): Boolean {
        val l = t.lowercase()
        return l.contains("://") || l.startsWith("http") || l.startsWith("www.")
    }

    private fun looksLikeEmail(t: String): Boolean {
        val parts = t.split("@")
        return parts.size == 2 && parts[0].isNotEmpty() &&
            parts[1].contains(".") && !parts[1].startsWith(".") && !parts[1].endsWith(".")
    }

    private fun looksLikePath(t: String): Boolean =
        t.startsWith("/") || t.startsWith("./") || t.startsWith("../") ||
            (t.length >= 3 && t[1] == ':' && (t[2] == '\\' || t[2] == '/'))

    private fun isPasswordLike(t: String): Boolean {
        val n = t.length
        if (n < 8 || n > 64 || t.any { it.isWhitespace() }) return false
        if (looksLikeUrl(t) || looksLikeEmail(t) || looksLikePath(t)) return false
        var lo = false
        var up = false
        var di = false
        var sy = false
        for (c in t) {
            when {
                c in 'a'..'z' -> lo = true
                c in 'A'..'Z' -> up = true
                c in '0'..'9' -> di = true
                !c.isLetterOrDigit() -> sy = true
            }
        }
        val classes = listOf(lo, up, di, sy).count { it }
        return classes >= 3 && shannonBitsPerChar(t) >= 2.5
    }

    private fun shannonBitsPerChar(s: String): Double {
        if (s.isEmpty()) return 0.0
        val counts = HashMap<Char, Int>()
        for (c in s) counts[c] = (counts[c] ?: 0) + 1
        val n = s.length.toDouble()
        var h = 0.0
        for (cnt in counts.values) {
            val p = cnt / n
            h -= p * (ln(p) / ln(2.0))
        }
        return h
    }
}
