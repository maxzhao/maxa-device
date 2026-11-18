package com.maxa.device.companion

import org.junit.Assert.*
import org.junit.Test

class ProtocolTest {
    @Test fun frozenCrossImplementationVector() {
        val secret = ByteArray(32) { (it + 1).toByte() }
        val client = "00".repeat(32); val server = "11".repeat(32)
        val serverProof = Protocol.proof(secret, "server", client, server)
        val clientProof = Protocol.proof(secret, "client", server, client)
        val key = Protocol.hmac(secret, "session", client, server)
        val requestMac = Protocol.proof(key, "req", "1", Protocol.sha("{}".toByteArray()))
        println("serverProof=$serverProof\nclientProof=$clientProof\nsessionKey=${Protocol.hex(key)}\nrequestMac=$requestMac")
        assertEquals("3cff9123aaa9d7f3c06a26002bc007e2e4b26d23d74d2438d75f2f2469a69a34", serverProof)
        assertEquals("aa35fa3c58af433fb6938b66353e3acd289e3a9b77dae08cef62181a7cddaa8b", clientProof)
        assertEquals("c889f83a7ebd29933dfbfa27f8e15eeb06b781c306a9889185748e8b7af45737", Protocol.hex(key))
        assertEquals("191d1c5ec42f717316898641db24e03b5243199cd953acd859a79bfb3c03860f", requestMac)
        assertFalse(Protocol.equalHex(serverProof, "00".repeat(32)))
    }
    @Test fun exactInstallCommand() {
        val secret = java.util.Base64.getUrlEncoder().withoutPadding().encodeToString(ByteArray(32) { (it + 1).toByte() })
        val hash = "ab".repeat(32)
        val actual = InstallCommand.create(19428, hash, secret)
        assertEquals("curl -fsS http://127.0.0.1:19428/install/install.sh -o \"\$TMPDIR/maxa-device-install.sh\" && echo \"$hash  \$TMPDIR/maxa-device-install.sh\" | sha256sum -c --quiet && sh \"\$TMPDIR/maxa-device-install.sh\" --port 19428 --secret $secret", actual)
        for (invalid in listOf("secret;exec", "x".repeat(42), "x".repeat(44))) {
            assertThrows(IllegalArgumentException::class.java) { InstallCommand.create(19428, hash, invalid) }
        }
        assertThrows(IllegalArgumentException::class.java) { InstallCommand.create(0, hash, secret) }
        assertThrows(IllegalArgumentException::class.java) { InstallCommand.create(19428, "bad", secret) }
    }
}
