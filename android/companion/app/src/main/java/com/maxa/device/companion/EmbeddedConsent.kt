package com.maxa.device.companion

import android.app.Activity
import android.app.AlertDialog
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.Settings
import android.view.WindowManager
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import org.json.JSONObject
import java.util.concurrent.Executors

internal object EmbeddedConsent {
    private val handler = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()
    internal class Request(val id: String, val name: String, val owner: String, val epoch: Long,
                           val expires: Long, val args: JSONObject, val description: String) {
        @Volatile var state = "awaiting_local_confirmation"
        var result: Any? = null
        var error: OpError? = null
    }
    private val requests = LinkedHashMap<String, Request>()
    @Volatile var protectedSurface = false; private set

    @Synchronized fun call(name: String, args: JSONObject, token: CallToken): JSONObject {
        token.check()
        prune()
        if (args.has("ticket")) {
            args.only("ticket", "cancel")
            val id = args.text("ticket", 32)
            val entry = requests[id]?.takeIf { it.name == name && it.epoch == token.epoch }
                ?: fail("NOT_FOUND", "Confirmation ticket missing or expired; do not replay an unknown outcome")
            if (args.bool("cancel", false) && entry.state != "complete") {
                if (entry.state == "running") fail("BUSY", "Use Stop on the device; an approved operation may already have started")
                complete(entry, error = OpError("CANCELLED", "Cancelled before approval"))
            }
            entry.error?.let { throw it }
            return obj("state" to entry.state, "ticket" to entry.id, "value" to entry.result)
        }
        val description = EmbeddedProtected.describe(name, args)
        if (requests.values.any { it.state != "complete" } || protectedSurface) fail("BUSY", "There is already a local review or authorization in progress")
        if (requests.size >= 16) fail("BUSY", "Confirmation queue is full; wait for older tickets to expire")
        val id = Protocol.hex(Protocol.randomBytes(16))
        val entry = Request(id, name, token.owner, token.epoch, SystemClock.elapsedRealtime() + 180_000, args, description)
        requests[id] = entry
        handler.postDelayed({ synchronized(this) { prune() } }, 181_000)
        return obj("state" to entry.state, "ticket" to entry.id, "expiresInMs" to 180_000)
    }

    @Synchronized private fun prune() {
        requests.values.removeAll { it.expires <= SystemClock.elapsedRealtime() || it.epoch != CompanionRuntime.authEpoch }
    }
    @Synchronized fun review(): Request? {
        prune()
        return requests.values.firstOrNull { it.state == "awaiting_local_confirmation" }
    }
    // Same paired UID and link epoch, even when a new local CLI process negotiates a session.
    @Synchronized fun invalidate(name: String? = null) {
        requests.values.removeAll { name == null || it.name == name }
        MainThread.post { activity?.invalidate(name) }
    }
    private var activity: EmbeddedConsentActivity? = null
    @Synchronized fun begin(entry: Request) {
        checkLive(entry)
        if (entry.state != "awaiting_local_confirmation") fail("CANCELLED", "The request is no longer pending")
        entry.state = "running"
    }
    @Synchronized fun checkLive(entry: Request) {
        prune()
        if (requests[entry.id] !== entry || entry.state == "complete") fail("CANCELLED", "Confirmation expired, disconnected or cancelled")
        CompanionRuntime.gate("api.${entry.name}", CallToken(entry.owner, entry.id, entry.epoch))
    }
    @Synchronized fun complete(entry: Request, value: Any? = null, error: OpError? = null) {
        if (requests[entry.id] !== entry || entry.state == "complete") return
        entry.result = value
        entry.error = error
        entry.state = "complete"
    }
    fun work(activity: EmbeddedConsentActivity, entry: Request, task: () -> Any) {
        worker.execute {
            val result = runCatching { checkLive(entry); task() }
            // Never disclose provider errors, raw paths, or permission internals.
            MainThread.post { activity.finished(entry, result) }
        }
    }
    @Synchronized fun enter(surface: EmbeddedConsentActivity): Boolean {
        if (protectedSurface) return false
        activity = surface
        protectedSurface = true
        return true
    }
    @Synchronized fun leave() { activity = null; protectedSurface = false }
}

/** Non-exported. Remote RPC enqueues data only; a local activity must explicitly open this review. */
class EmbeddedConsentActivity : Activity() {
    companion object {
        internal const val PERMISSION = "embeddedPermission"
        internal fun launch(context: Context, capability: String?) {
            if (capability != null) {
                require(capability in EmbeddedApi.capabilityIds)
                if (!CompanionRuntime.enabled(capability)) fail("CAPABILITY_DISABLED", "Enable the capability first")
            }
            // Ensure queued/in-flight remote input cannot accept a system flow.
            if (CompanionRuntime.enabled("ui")) fail("HUMAN_CONFIRMATION_REQUIRED", "Turn off the remote UI capability before opening this local flow")
            if (CompanionRuntime.operations.recording.isActive()) fail("BUSY", "Stop remote screen recording first")
            context.startActivity(Intent(context, EmbeddedConsentActivity::class.java).putExtra(PERMISSION, capability))
        }
    }
    private var ownsSurface = false
    private var current: EmbeddedConsent.Request? = null
    private var external: ((Int, Intent?) -> Unit)? = null
    private val handler = Handler(Looper.getMainLooper())
    private var afterStop: (() -> Unit)? = null
    private lateinit var content: LinearLayout

    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        if (android.os.Build.VERSION.SDK_INT >= 31) window.setHideOverlayWindows(true)
        if (state != null || CompanionRuntime.enabled("ui") || CompanionRuntime.operations.recording.isActive() || !EmbeddedConsent.enter(this)) { finish(); return }
        ownsSurface = true
        content = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        setContentView(android.widget.ScrollView(this).apply { addView(content) })
        val permission = intent.getStringExtra(PERMISSION)
        if (permission != null) {
            val name = permission.removePrefix("api.")
            val permissions = EmbeddedProtected.permissions(name)
            text("Android authorization: $name")
            button("Open system authorization") {
                if (!CompanionRuntime.enabled(permission)) { finish(); return@button }
                val appUri = android.net.Uri.fromParts("package", packageName, null)
                val route = when (name) {
                    "notification-list" -> Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS)
                    "brightness" -> Intent(Settings.ACTION_MANAGE_WRITE_SETTINGS, appUri)
                    "notification", "notification-channel", "job-scheduler" ->
                        Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE, packageName)
                    else -> null
                }
                try {
                    if (route != null) openSystem(route) { _, _ -> finish() }
                    else if (permissions.isNotEmpty()) requestPermissions(permissions, 9)
                    else finish()
                } catch (_: android.content.ActivityNotFoundException) { text("This Android device has no matching settings page") }
            }
            button("Cancel") { finish() }
            return
        }
        val entry = EmbeddedConsent.review() ?: run { text("No pending request"); button("Close") { finish() }; return }
        current = entry
        text(entry.description)
        button("Continue") {
            try {
                EmbeddedConsent.begin(entry)
                content.removeAllViews(); text(entry.description)
                button("Stop / close") { finish() }
                EmbeddedProtected.execute(this, entry)
            } catch (e: Exception) { finished(entry, Result.failure(e)) }
        }
        button("Reject") { finish() }
        handler.postAtTime({ finish() }, SystemClock.uptimeMillis() + (entry.expires - SystemClock.elapsedRealtime()).coerceAtLeast(0))
    }

    internal fun text(value: String) { content.addView(TextView(this).apply { text = value }) }
    internal fun button(title: String, action: () -> Unit) {
        content.addView(Button(this).apply {
            text = title
            filterTouchesWhenObscured = true
            setOnClickListener { action() }
        })
    }
    internal fun openSystem(intent: Intent, cb: (Int, Intent?) -> Unit) {
        check(external == null)
        external = cb
        try { startActivityForResult(intent, 4) }
        catch (e: Exception) { external = null; throw e }
    }
    internal fun onStopAction(action: () -> Unit) { afterStop = action }
    internal fun confirm(entry: EmbeddedConsent.Request, title: String, message: String) =
        confirmAction(entry, title, message) { obj("confirmed" to true) }
    internal fun confirmAction(entry: EmbeddedConsent.Request, title: String, message: String, work: () -> Any) {
        val dialog = AlertDialog.Builder(this).setTitle(title).setMessage(message)
            .setPositiveButton(android.R.string.ok) { _, _ -> EmbeddedConsent.work(this, entry, task = work) }
            .setNegativeButton(android.R.string.cancel) { _, _ -> finish() }.setOnCancelListener { finish() }.create()
        dialog.window?.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        dialog.setOnShowListener { dialog.getButton(AlertDialog.BUTTON_POSITIVE).filterTouchesWhenObscured = true }
        dialog.show()
    }
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == 4) {
            val cb = external; external = null
            try { cb?.invoke(resultCode, data) }
            catch (e: Exception) { current?.let { finished(it, Result.failure(e)) } ?: finish() }
        }
    }
    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == 9) finish()
    }
    internal fun invalidate(name: String?) { if (name == null || current?.name == name) finish() }
    internal fun finished(entry: EmbeddedConsent.Request, result: Result<Any>) {
        if (isFinishing || isDestroyed || current !== entry) return
        val e = result.exceptionOrNull()
        if (e == null) {
            try { EmbeddedConsent.checkLive(entry) } catch (_: OpError) { finish(); return }
        }
        val error = when (e) {
            null -> null
            is OpError -> e
            is SecurityException -> OpError("PERMISSION_MISSING", "Android denied the selected operation")
            is android.content.ActivityNotFoundException -> OpError("PLATFORM_RESTRICTED", "No system activity handles this operation")
            else -> OpError("OUTCOME_UNKNOWN", "Android did not confirm the result; do not replay")
        }
        EmbeddedConsent.complete(entry, result.getOrNull(), error)
        current = null
        finish()
    }
    override fun onStop() {
        super.onStop()
        if (external == null) afterStop?.invoke()
    }
    override fun onDestroy() {
        handler.removeCallbacksAndMessages(null)
        afterStop?.invoke(); afterStop = null
        current?.let { EmbeddedConsent.complete(it, error = if (it.state == "running")
            OpError("OUTCOME_UNKNOWN", "Local review closed after approval; do not replay") else OpError("CANCELLED", "Local user declined or dismissed the request")) }
        if (ownsSurface) EmbeddedConsent.leave()
        super.onDestroy()
    }
}
