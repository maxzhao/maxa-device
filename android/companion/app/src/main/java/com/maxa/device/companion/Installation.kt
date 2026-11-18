package com.maxa.device.companion

import android.app.*
import android.content.*
import android.content.pm.PackageInstaller
import android.os.*
import org.json.JSONObject
import java.util.concurrent.*

class InstallationManager(private val context: Context) {
    data class Pending(val id: String, val token: CallToken, val deadline: Long, val result: CompletableFuture<JSONObject> = CompletableFuture())
    private val pending = ConcurrentHashMap<String, Pending>()
    private val busy = java.util.concurrent.atomic.AtomicBoolean(false)
    private fun sender(p: Pending): IntentSender {
        val intent = Intent(context, InstallReceiver::class.java).setAction("${context.packageName}.INSTALL_RESULT").putExtra("operation", p.id)
        val pi = PendingIntent.getBroadcast(context, p.id.hashCode(), intent, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE)
        return pi.intentSender
    }
    private fun operation(token: CallToken, submit: (Pending) -> Unit): JSONObject {
        CompanionRuntime.gate("install", token)
        if (!busy.compareAndSet(false, true)) fail("BUSY", "An installation or uninstallation is already pending")
        val id = Protocol.hex(Protocol.randomBytes(12)); val p = Pending(id, token, SystemClock.elapsedRealtime() + 180_000)
        pending[id] = p
        var submitted = false
        try {
            token.check(); submit(p); submitted = true
            while (SystemClock.elapsedRealtime() < p.deadline) {
                if (token.cancelled.get() || token.epoch != CompanionRuntime.authEpoch) fail("OUTCOME_UNKNOWN", "Confirmation wait cancelled after submission; installation outcome unknown")
                try { return p.result.get(200, TimeUnit.MILLISECONDS) } catch (_: TimeoutException) {}
            }
            return obj("status" to "unknown", "message" to "Human confirmation timed out; no retry")
        } catch (e: Exception) {
            if (submitted && e !is OpError) fail("OUTCOME_UNKNOWN", "PackageInstaller outcome unknown; no retry")
            throw e
        } finally { pending.remove(id); busy.set(false) }
    }
    fun install(blobId: String, token: CallToken): JSONObject {
        CompanionRuntime.gate("install", token)
        val blob = CompanionRuntime.blobs.get(blobId, token.owner)
        if (blob.capability != "install" || blob.written != blob.expected || blob.written == 0L) fail("INVALID_ARGUMENT", "Complete install upload required")
        return operation(token) { p ->
            val installer = context.packageManager.packageInstaller
            val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL)
            params.setSize(blob.file.length())
            if (Build.VERSION.SDK_INT >= 31) params.setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_REQUIRED)
            val sessionId = installer.createSession(params)
            try {
                installer.openSession(sessionId).use { session ->
                    session.openWrite("base.apk", 0, blob.file.length()).use { output ->
                        blob.file.inputStream().use { input ->
                            val buffer = ByteArray(1024 * 1024)
                            while (true) { token.check(); val count = input.read(buffer); if (count < 0) break; output.write(buffer, 0, count) }
                        }; session.fsync(output)
                    }
                    token.check(); session.commit(sender(p))
                }
            } catch (e: Exception) { try { installer.abandonSession(sessionId) } catch (_: Exception) {}; throw e }
        }
    }
    fun uninstall(packageName: String, token: CallToken): JSONObject {
        if (!packageName.matches(Regex("[A-Za-z][A-Za-z0-9_]*(\\.[A-Za-z0-9_]+)+"))) fail("INVALID_ARGUMENT", "Invalid packageName")
        return operation(token) { p -> context.packageManager.packageInstaller.uninstall(packageName, sender(p)) }
    }
    fun received(intent: Intent) {
        val p = intent.getStringExtra("operation")?.let(pending::get) ?: return
        val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)
        if (status == PackageInstaller.STATUS_PENDING_USER_ACTION) {
            val confirmation = intent.getParcelableExtra<Intent>(Intent.EXTRA_INTENT)
            if (confirmation == null) { p.result.complete(obj("status" to "unknown", "message" to "System confirmation missing")); return }
            Thread({
                try { Prompts.request("installation", confirmation, p.token, maxOf(1, (p.deadline - SystemClock.elapsedRealtime()) / 1000)) }
                catch (_: Exception) { /* Real PackageInstaller status remains authoritative. */ }
            }, "human-install-confirmation").start()
        } else {
            val result = when (status) { PackageInstaller.STATUS_SUCCESS -> "installed"; PackageInstaller.STATUS_FAILURE_ABORTED -> "rejected"; else -> "rejected" }
            p.result.complete(obj("status" to result, "message" to if (result == "installed") "Package operation completed" else "Package operation rejected or failed (status $status)"))
        }
    }
}
class InstallReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) { CompanionRuntime.operations.installation.received(intent) }
}
