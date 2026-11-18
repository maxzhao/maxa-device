package com.maxa.device.companion

import java.security.MessageDigest
import java.security.SecureRandom
import java.util.Base64
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec

object Protocol {
    val random = SecureRandom()
    fun randomBytes(n: Int) = ByteArray(n).also { random.nextBytes(it) }
    fun hex(bytes: ByteArray) = bytes.joinToString("") { "%02x".format(it.toInt() and 255) }
    fun unhex(value: String): ByteArray {
        require(value.matches(Regex("[0-9a-f]+")) && value.length % 2 == 0)
        return value.chunked(2).map { it.toInt(16).toByte() }.toByteArray()
    }
    fun sha(bytes: ByteArray) = hex(MessageDigest.getInstance("SHA-256").digest(bytes))
    fun hmac(key: ByteArray, vararg parts: String): ByteArray = Mac.getInstance("HmacSHA256").run {
        init(SecretKeySpec(key, "HmacSHA256")); doFinal(parts.joinToString("\u0000").toByteArray(Charsets.UTF_8))
    }
    fun proof(key: ByteArray, vararg parts: String) = hex(hmac(key, *parts))
    fun equalHex(expected: String, supplied: String): Boolean = try {
        MessageDigest.isEqual(unhex(expected), unhex(supplied))
    } catch (_: IllegalArgumentException) { false }
    fun secret() = Base64.getUrlEncoder().withoutPadding().encodeToString(randomBytes(32))
    fun decodeSecret(secret: String): ByteArray = Base64.getUrlDecoder().decode(secret).also { require(it.size == 32) }
}

object InstallCommand {
    fun create(port: Int, installSha256: String, secret: String): String {
        require(port in 1..65535 && installSha256.matches(Regex("[0-9a-f]{64}")))
        require(secret.matches(Regex("[A-Za-z0-9_-]{43}")))
        Protocol.decodeSecret(secret)
        return "curl -fsS http://127.0.0.1:$port/install/install.sh -o \"\$TMPDIR/maxa-device-install.sh\" && echo \"$installSha256  \$TMPDIR/maxa-device-install.sh\" | sha256sum -c --quiet && sh \"\$TMPDIR/maxa-device-install.sh\" --port $port --secret $secret"
    }
}
