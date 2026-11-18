package com.maxa.device.companion

import android.content.Context
import android.os.SystemClock
import android.util.Log
import org.json.JSONObject
import java.io.*
import java.net.*
import java.util.Locale
import java.util.concurrent.*

class LoopbackServer(private val context: Context) {
    data class Session(val id: String, val client: String, val server: String, val key: ByteArray, val epoch: Long,
        var confirmed: Boolean = false, var seq: Long = 0, var touched: Long = SystemClock.elapsedRealtime())
    private val sessions = linkedMapOf<String, Session>()
    @Volatile var port: Int = 0; private set
    private var listener: ServerSocket? = null
    private var workers: ThreadPoolExecutor? = null
    private var cleaner: ScheduledExecutorService? = null
    @Synchronized fun start() {
        if (listener != null) return
        val address = InetAddress.getByName("127.0.0.1")
        val socket = try { ServerSocket(19428, 16, address) } catch (_: BindException) { ServerSocket(0, 16, address) }
        check(socket.inetAddress.hostAddress == "127.0.0.1")
        listener = socket; port = socket.localPort
        val pool = ThreadPoolExecutor(12, 12, 30, TimeUnit.SECONDS, ArrayBlockingQueue(32)); workers = pool
        cleaner = Executors.newSingleThreadScheduledExecutor().also { it.scheduleAtFixedRate({ CompanionRuntime.blobs.cleanup(); expire() }, 60, 60, TimeUnit.SECONDS) }
        Thread({ while (!socket.isClosed) {
            try { val client = socket.accept(); client.soTimeout = 15_000
                try { pool.execute { client.use { handle(it) } } } catch (_: RejectedExecutionException) { client.close() }
            } catch (_: IOException) { if (socket.isClosed) break }
        } }, "companion-loopback").start()
    }
    @Synchronized fun stop() { listener?.close(); listener = null; port = 0; workers?.shutdownNow(); cleaner?.shutdownNow(); invalidate() }
    @Synchronized fun invalidate() { sessions.keys.toList().forEach { drop(it) } }
    @Synchronized private fun drop(id: String) {
        sessions.remove(id); CompanionRuntime.linked = sessions.values.any { it.confirmed }
        CompanionRuntime.calls.values.filter { it.owner == id }.forEach { it.cancelled.set(true) }
        CompanionRuntime.blobs.removeOwner(id); CompanionRuntime.operations.recording.cancelOwner(id)
    }
    @Synchronized private fun expire() {
        val now = SystemClock.elapsedRealtime()
        sessions.values.filter { now - it.touched > 600_000 }.map { it.id }.forEach { drop(it) }
    }
    private data class Request(val method: String, val path: String, val headers: Map<String, String>, val body: ByteArray)
    private fun readRequest(input: InputStream): Request {
        var total = 0
        fun line(): String {
            val bytes = ByteArrayOutputStream()
            while (true) {
                val b = input.read(); if (b < 0 || ++total > 16384) throw IOException("Invalid HTTP headers")
                if (b == 10) { val data = bytes.toByteArray(); if (data.isEmpty() || data.last() != 13.toByte()) throw IOException("CRLF required"); return String(data, 0, data.size - 1, Charsets.US_ASCII) }
                bytes.write(b)
            }
        }
        val head = line().split(' '); if (head.size != 3 || head[2] != "HTTP/1.1") throw IOException("HTTP/1.1 required")
        val headers = mutableMapOf<String, String>()
        while (true) {
            val line = line(); if (line.isEmpty()) break
            val split = line.indexOf(':'); if (split <= 0) throw IOException("Invalid header")
            val key = line.substring(0, split).lowercase(Locale.ROOT)
            if (headers.put(key, line.substring(split + 1).trim()) != null) throw IOException("Duplicate header")
        }
        if (headers.containsKey("transfer-encoding")) throw IOException("Chunked requests not supported")
        val length = headers["content-length"]?.toIntOrNull() ?: if (head[0] == "GET") 0 else throw IOException("Content-Length required")
        if (length !in 0..1048576) throw IOException("Body too large")
        val body = ByteArray(length); DataInputStream(input).readFully(body)
        return Request(head[0], head[1], headers, body)
    }
    private fun send(output: OutputStream, status: Int, bytes: ByteArray, headers: Map<String, String> = emptyMap()) {
        output.write(("HTTP/1.1 $status ${if (status in 200..299) "OK" else "Error"}\r\nContent-Length: ${bytes.size}\r\nConnection: close\r\n" + headers.entries.joinToString("") { "${it.key}: ${it.value}\r\n" } + "\r\n").toByteArray(Charsets.US_ASCII))
        output.write(bytes); output.flush()
    }
    private class ResponseOutput(private val target: OutputStream) : OutputStream() {
        var started = false; private set
        override fun write(value: Int) { started = true; target.write(value) }
        override fun write(bytes: ByteArray, offset: Int, length: Int) {
            if (length > 0) started = true
            target.write(bytes, offset, length)
        }
        override fun flush() { target.flush() }
    }
    private fun handle(socket: Socket) {
        val output = ResponseOutput(socket.getOutputStream())
        try {
            val request = readRequest(socket.getInputStream())
            if (request.method == "GET" && request.path in setOf("/install/manifest.json", "/install/install.sh", "/install/agent.tar.gz")) {
                val name = if (request.path == "/install/agent.tar.gz") "agent.payload" else request.path.substringAfterLast('/')
                // Asset stream is the only size-limit exception. Never maps arbitrary paths to storage.
                context.assets.openFd(name).use { asset ->
                    // Open before committing 200, so an asset-open failure can still return one valid 400.
                    asset.createInputStream().use { input ->
                        output.write("HTTP/1.1 200 OK\r\nContent-Length: ${asset.length}\r\nConnection: close\r\nContent-Type: application/octet-stream\r\n\r\n".toByteArray())
                        input.copyTo(output); output.flush()
                    }
                }; return
            }
            if (request.method != "POST") { send(output, 404, ByteArray(0)); return }
            when (request.path) {
                "/v1/session" -> begin(request, output)
                "/v1/session/confirm" -> confirm(request, output)
                "/v1/call" -> call(request, output)
                else -> send(output, 404, ByteArray(0))
            }
        } catch (e: Exception) {
            // JSONException messages can contain the submitted JSON. Never log bodies, auth headers or secrets.
            val message = (if (e is org.json.JSONException) "Malformed JSON request (details withheld)" else e.message ?: "No exception message")
                .replace('\r', ' ').replace('\n', ' ').take(256)
            Log.w("MaxaCompanion", "HTTP ${if (output.started) "response failed" else "400"}: ${e.javaClass.simpleName}: $message")
            // Once any response byte was attempted, appending another HTTP response would corrupt the payload.
            if (!output.started) try { send(output, 400, ByteArray(0)) } catch (_: IOException) {}
        }
    }
    @Synchronized private fun begin(request: Request, out: OutputStream) {
        val secret = CompanionRuntime.secret() ?: run { send(out, 401, ByteArray(0)); return }
        val client = JSONObject(String(request.body, Charsets.UTF_8)).getString("clientNonce")
        if (!client.matches(Regex("[0-9a-f]{64}"))) { send(out, 401, ByteArray(0)); return }
        expire(); while (sessions.size >= 4) drop(sessions.keys.first())
        val server = Protocol.hex(Protocol.randomBytes(32)); val id = Protocol.hex(Protocol.randomBytes(16))
        sessions[id] = Session(id, client, server, Protocol.hmac(secret, "session", client, server), CompanionRuntime.authEpoch)
        val response = obj("sessionId" to id, "serverNonce" to server, "serverProof" to Protocol.proof(secret, "server", client, server))
        send(out, 200, response.toString().toByteArray())
    }
    @Synchronized private fun confirm(request: Request, out: OutputStream) {
        expire(); val json = JSONObject(String(request.body, Charsets.UTF_8)); val id = json.getString("sessionId")
        val session = sessions[id]; val secret = CompanionRuntime.secret()
        if (session == null || session.confirmed || secret == null || session.epoch != CompanionRuntime.authEpoch || !Protocol.equalHex(Protocol.proof(secret, "client", session.server, session.client), json.optString("clientProof"))) {
            drop(id); send(out, 401, ByteArray(0)); return
        }
        session.confirmed = true; session.touched = SystemClock.elapsedRealtime(); CompanionRuntime.linked = true
        send(out, 204, ByteArray(0))
    }
    private fun call(request: Request, out: OutputStream) {
        val id = request.headers["x-maxa-session"] ?: ""; val seqString = request.headers["x-maxa-seq"] ?: ""
        val seq = seqString.toLongOrNull(); val meta = request.headers["x-maxa-meta"]
        val session = synchronized(this) {
            expire(); val candidate = sessions[id]
            val parts = mutableListOf("req", seqString, Protocol.sha(request.body)); if (meta != null) parts.add(meta)
            if (candidate == null || !candidate.confirmed || candidate.epoch != CompanionRuntime.authEpoch || seq == null || (candidate.seq == 0L && seq != 1L) || seq <= candidate.seq || !seqString.matches(Regex("[1-9][0-9]*")) || !Protocol.equalHex(Protocol.proof(candidate.key, *parts.toTypedArray()), request.headers["x-maxa-mac"] ?: "")) null
            else candidate.also { it.seq = seq; it.touched = SystemClock.elapsedRealtime() }
        }
        if (session == null) { send(out, 401, ByteArray(0)); return }
        var read: BlobStore.Read? = null
        var revoke = false
        var token: CallToken? = null
        val response: ByteArray
        var responseMeta: String? = null
        try {
            val json = JSONObject(meta ?: String(request.body, Charsets.UTF_8))
            val op = json.getString("op"); val args = json.optJSONObject("args") ?: JSONObject()
            if (meta != null && op != "blob.write") fail("INVALID_ARGUMENT", "Raw metadata only supported for blob.write")
            if (meta == null && op == "blob.write") fail("INVALID_ARGUMENT", "blob.write requires raw body and metadata")
            val requestId = json.getString("requestId"); if (requestId.isEmpty() || requestId.length > 128) fail("INVALID_ARGUMENT", "Invalid requestId")
            val current = CallToken(session.id, requestId, session.epoch); token = current
            if (CompanionRuntime.calls.putIfAbsent("$id:$requestId", current) != null) fail("BUSY", "requestId already in flight")
            current.check()
            when (op) {
                "blob.read" -> {
                    val blob = CompanionRuntime.blobs.get(args.getString("blobId"), id)
                    CompanionRuntime.gate(blob.capability, current)
                    read = CompanionRuntime.blobs.read(id, blob.id, args.getLong("offset"), args.getInt("length"))
                    response = read!!.bytes; responseMeta = obj("ok" to true, "result" to obj("eof" to read!!.eof)).toString()
                }
                else -> {
                    val result = CompanionRuntime.operations.dispatch(op, args, current, if (meta != null) request.body else null)
                    revoke = op == "link.revoke"
                    response = obj("ok" to true, "result" to result).toString().toByteArray()
                }
            }
            if (response.size > 1048576) fail("INTERNAL", "Response exceeds 1 MiB")
        } catch (e: Exception) {
            val error = when (e) {
                is OpError -> e
                is org.json.JSONException, is IllegalArgumentException -> OpError("INVALID_ARGUMENT", "Malformed operation arguments")
                is TimeoutException -> OpError("OUTCOME_UNKNOWN", "Operation timed out; do not replay")
                is SecurityException -> OpError("PERMISSION_MISSING", "Android permission denied")
                else -> OpError("OUTCOME_UNKNOWN", "Operation failed; outcome may be unknown; do not replay")
            }
            val bytes = obj("ok" to false, "error" to obj("code" to error.code, "message" to error.message)).toString().toByteArray()
            signedSend(out, session, seqString, bytes, null); return
        } finally {
            token?.let { CompanionRuntime.calls.remove("$id:${it.requestId}", it) }
            synchronized(this) { session.touched = SystemClock.elapsedRealtime() }
        }
        try { signedSend(out, session, seqString, response, responseMeta) }
        finally { if (revoke) CompanionRuntime.unlink() }
        read?.takeIf { it.eof }?.let { CompanionRuntime.blobs.remove(it.id) }
    }
    private fun signedSend(out: OutputStream, session: Session, seq: String, body: ByteArray, meta: String?) {
        val parts = mutableListOf("resp", seq, Protocol.sha(body)); if (meta != null) parts.add(meta)
        val headers = mutableMapOf("X-Maxa-Mac" to Protocol.proof(session.key, *parts.toTypedArray()), "Content-Type" to if (meta == null) "application/json" else "application/octet-stream")
        if (meta != null) headers["X-Maxa-Meta"] = meta
        send(out, 200, body, headers)
    }
}
