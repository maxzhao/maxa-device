package com.maxa.device.companion

import android.app.*
import android.content.*
import android.media.projection.MediaProjectionManager
import android.os.*
import android.widget.TextView
import java.util.concurrent.*
import java.util.concurrent.atomic.AtomicBoolean

object Prompts {
    data class Prompt(val id: String, val type: String, val intent: Intent?, val token: CallToken, val result: CompletableFuture<Pair<Int, Intent?>> = CompletableFuture())
    private val prompts = ConcurrentHashMap<String, Prompt>()
    fun get(id: String) = prompts[id]
    fun request(type: String, intent: Intent?, token: CallToken, seconds: Long): Pair<Int, Intent?> {
        token.check(); val id = Protocol.hex(Protocol.randomBytes(12)); val prompt = Prompt(id, type, intent, token)
        prompts[id] = prompt
        val context = CompanionRuntime.app
        val launch = Intent(context, ConsentActivity::class.java).putExtra("prompt", id).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        val notificationId = id.hashCode()
        MainThread.call {
            token.check()
            val open = PendingIntent.getActivity(context, notificationId, launch, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
            val notification = Notification.Builder(context, "companion").setSmallIcon(android.R.drawable.ic_dialog_info).setContentTitle("Maxa: human confirmation needed").setContentText("Open companion to confirm $type; automation cannot operate confirmation screens").setContentIntent(open).setAutoCancel(true).build()
            context.getSystemService(NotificationManager::class.java).notify(notificationId, notification)
            // Android background-start restrictions may require the human to tap the notification.
            try { context.startActivity(launch) } catch (_: Exception) {}
        }
        val deadline = SystemClock.elapsedRealtime() + seconds * 1000
        try {
            while (SystemClock.elapsedRealtime() < deadline) {
                token.check()
                try { return prompt.result.get(200, TimeUnit.MILLISECONDS) } catch (_: TimeoutException) {}
            }
            fail("OUTCOME_UNKNOWN", "Human $type confirmation timed out; no automatic retry")
        } finally { prompts.remove(id); context.getSystemService(NotificationManager::class.java).cancel(notificationId) }
    }
}

class ConsentActivity : Activity() {
    companion object { val projectionPending = AtomicBoolean(false) }
    private var promptId: String? = null
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        promptId = intent.getStringExtra("prompt")
        val prompt = promptId?.let(Prompts::get) ?: run { finish(); return }
        setContentView(TextView(this).apply { text = "Confirm using Android's system UI. Maxa cannot operate this confirmation."; setPadding(24, 24, 24, 24) })
        if (savedInstanceState != null) return
        try {
            prompt.token.check()
            val target = if (prompt.type == "recording") {
                projectionPending.set(true)
                getSystemService(MediaProjectionManager::class.java).createScreenCaptureIntent()
            } else prompt.intent ?: throw IllegalArgumentException("Missing confirmation intent")
            startActivityForResult(target, 71)
        } catch (e: Exception) { projectionPending.set(false); prompt.result.completeExceptionally(e); finish() }
    }
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == 71) {
            projectionPending.set(false)
            promptId?.let(Prompts::get)?.result?.complete(resultCode to data)
            finish()
        }
    }
    override fun onDestroy() {
        if (isFinishing) projectionPending.set(false)
        super.onDestroy()
    }
}

object ClipboardBridge {
    data class Pending(val id: String, val text: String?, val token: CallToken, val future: CompletableFuture<String?> = CompletableFuture())
    private val requests = ConcurrentHashMap<String, Pending>()
    fun pending(id: String) = requests[id]
    fun access(text: String?, token: CallToken): String? {
        val id = Protocol.hex(Protocol.randomBytes(12)); val pending = Pending(id, text, token); requests[id] = pending
        try {
            MainThread.call { token.check(); CompanionRuntime.app.startActivity(Intent(CompanionRuntime.app, ClipboardActivity::class.java).putExtra("request", id).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) }
            return try { pending.future.get(8, TimeUnit.SECONDS) } catch (_: TimeoutException) { fail("UNSUPPORTED", "clipboard not readable; no write sent") } catch (e: ExecutionException) { throw (e.cause ?: e) }
        } finally { requests.remove(id) }
    }
}

/** A distinct temporary task gives clipboard focus; finishing returns to the previous foreground task. */
class ClipboardActivity : Activity() {
    private var handled = false
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState); setContentView(TextView(this).apply { text = "Maxa clipboard access — returning to previous app" })
    }
    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus || handled) return
        handled = true
        val pending = intent.getStringExtra("request")?.let(ClipboardBridge::pending)
        try {
            if (pending != null) {
                CompanionRuntime.gate("clipboard", pending.token)
                val manager = getSystemService(ClipboardManager::class.java)
                fun read(): String? = manager.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString()
                val before = read() ?: fail("UNSUPPORTED", "clipboard not readable; no write sent")
                if (pending.text != null) {
                    pending.token.check(); manager.setPrimaryClip(ClipData.newPlainText("Maxa", pending.text))
                    if (read() != pending.text) fail("OUTCOME_UNKNOWN", "Clipboard read-back mismatch; do not replay")
                }
                pending.future.complete(if (pending.text == null) before else pending.text)
            }
        } catch (e: Exception) { pending?.future?.completeExceptionally(e) }
        finally { finishAndRemoveTask() }
    }
}
