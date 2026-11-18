package com.maxa.device.companion

import android.app.*
import android.app.usage.UsageStatsManager
import android.content.*
import android.content.pm.PackageManager
import android.net.Uri
import android.os.*
import android.provider.Settings
import android.widget.*
import org.json.JSONObject
import java.util.concurrent.*
import java.util.concurrent.atomic.AtomicBoolean

class OpError(val code: String, override val message: String) : Exception(message)
fun fail(code: String, message: String): Nothing = throw OpError(code, message)
fun obj(vararg values: Pair<String, Any?>) = JSONObject().also { o -> values.forEach { o.put(it.first, it.second ?: JSONObject.NULL) } }

class CallToken(val owner: String, val requestId: String, val epoch: Long) {
    val cancelled = AtomicBoolean(false)
    fun check() { if (cancelled.get() || epoch != CompanionRuntime.authEpoch) fail("CANCELLED", "Call cancelled or link invalidated") }
}

object MainThread {
    private val handler = Handler(Looper.getMainLooper())
    fun <T> call(block: () -> T): T {
        if (Looper.myLooper() == Looper.getMainLooper()) return block()
        val task = FutureTask(block); handler.post(task)
        return try { task.get(10, TimeUnit.SECONDS) }
        catch (e: ExecutionException) { throw (e.cause ?: e) }
        catch (e: TimeoutException) { task.cancel(false); throw e }
    }
    fun post(block: () -> Unit) { handler.post(block) }
}

data class CapabilityState(val enabled: Boolean, val granted: Boolean, val ready: Boolean, val missing: String?) {
    fun json() = obj("enabled" to enabled, "granted" to granted, "ready" to ready, "missing" to missing)
}
data class TermuxState(val installed: Boolean) {
    val ready get() = installed
}
data class CompanionState(
    val version: String, val protocol: Int, val port: Int?, val payloadReady: Boolean,
    val termux: TermuxState, val linked: Boolean, val accessibility: Boolean,
    val capabilities: Map<String, CapabilityState>,
    val accessibilityEnabledInSystem: Boolean = false,
    val batteryExempt: Boolean = false,
    val agentInstalled: Boolean? = if (linked) true else null,
    val agentRunning: Boolean? = if (linked) true else null,
    val pairingState: String = "Unknown to APK; inspect locally in Termux CLI",
    val trustNotice: String = "All software installed or run in Termux shares its UID and can access host identity, pairing credentials and enabled companion capabilities.",
    val backgroundAdvice: String = "Optionally run termux-wake-lock, exempt Termux from battery optimization, and configure Termux:Boot/termux-services yourself. Android may still kill processes; Android 12+ child limits may need a human developer-settings change."
)

class CompanionApp : Application() {
    override fun onCreate() { super.onCreate(); CompanionRuntime.init(this) }
}

object CompanionRuntime {
    lateinit var app: Application; private set
    private lateinit var prefs: android.content.SharedPreferences
    val capabilities = listOf("ui", "recording", "install", "orientation", "clipboard", "usage") + EmbeddedApi.capabilityIds
    @Volatile var authEpoch = 0L; private set
    @Volatile var linked = false
    val calls = ConcurrentHashMap<String, CallToken>()
    /** Shows on the listener notification whether control is currently keeping the screen on. */
    fun controlActive(active: Boolean) {
        app.getSystemService(NotificationManager::class.java).notify(10, foregroundNotification(app, if (active) CONTROL_TEXT else LISTENER_TEXT))
    }
    lateinit var blobs: BlobStore; private set
    lateinit var server: LoopbackServer; private set
    lateinit var operations: Operations; private set
    fun init(application: Application) {
        app = application; prefs = app.getSharedPreferences("companion", Context.MODE_PRIVATE)
        RetiredEmbeddedApiMigration.run(app, prefs)
        blobs = BlobStore(app); operations = Operations(app); server = LoopbackServer(app)
    }
    fun enabled(name: String) = prefs.getBoolean("cap.$name", false)
    fun secret(): ByteArray? = prefs.getString("link.secret", null)?.let(Protocol::decodeSecret)
    @Synchronized fun regenerate(): String {
        unlink(); val value = Protocol.secret()
        check(prefs.edit().putString("link.secret", value).commit())
        return value
    }
    @Synchronized fun unlink() {
        authEpoch++; linked = false; server.invalidate(); calls.values.forEach { it.cancelled.set(true) }
        blobs.clear(); operations.recording.invalidate(); operations.embeddedApi.invalidate(); EmbeddedConsent.invalidate(); EmbeddedReminder.cancel(app)
        check(prefs.edit().remove("link.secret").commit())
    }
    fun setEnabled(name: String, enabled: Boolean) {
        require(name in capabilities)
        check(prefs.edit().putBoolean("cap.$name", enabled).commit())
        if (!enabled && name == "recording") operations.recording.invalidate()
        if (!enabled && name.startsWith("api.")) EmbeddedConsent.invalidate(name.removePrefix("api."))
        if (!enabled && name == "api.job-scheduler") EmbeddedReminder.cancel(app)
        if (!enabled && name == "api.torch") operations.embeddedApi.invalidate()
    }
    /** True when the system lists this service as enabled, even if it is not currently connected. */
    fun accessibilityEnabledInSystem(): Boolean {
        val enabled = Settings.Secure.getString(app.contentResolver, Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES) ?: return false
        val component = ComponentName(app, CompanionAccessibility::class.java)
        return enabled.split(':').any { ComponentName.unflattenFromString(it) == component }
    }
    fun batteryExempt(): Boolean = app.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(app.packageName)
    fun capability(name: String): CapabilityState {
        EmbeddedApi.capability(name, app, enabled(name))?.let { return it }
        val granted = when (name) {
            "ui" -> CompanionAccessibility.instance != null
            "install" -> app.packageManager.canRequestPackageInstalls()
            "orientation" -> Settings.System.canWrite(app)
            "usage" -> app.getSystemService(AppOpsManager::class.java).unsafeCheckOpNoThrow(AppOpsManager.OPSTR_GET_USAGE_STATS, android.os.Process.myUid(), app.packageName) == AppOpsManager.MODE_ALLOWED
            else -> true // recording always needs a fresh human consent at record.start
        }
        val enabled = enabled(name)
        val missing = when {
            !enabled -> "Capability disabled in companion"
            !granted && name == "ui" && accessibilityEnabledInSystem() -> "Accessibility is enabled in system settings but the service is not connected; another UiAutomation client (such as Windows ADB automation) may be suppressing it, or autostart is blocked. Stop that client, then turn the service off and on."
            !granted && name == "ui" -> "Enable accessibility manually; sideloaded apps may first need Allow restricted settings (Android 13+)"
            !granted -> "Grant $name special access manually"
            else -> null
        }
        return CapabilityState(enabled, granted, enabled && granted, missing)
    }
    fun gate(name: String, token: CallToken) {
        token.check(); val state = capability(name)
        if (!state.enabled) fail("CAPABILITY_DISABLED", state.missing!!)
        if (!state.granted) fail(if (name == "ui") "ACCESSIBILITY_DISABLED" else "PERMISSION_MISSING", state.missing!!)
    }
    fun status() = obj("version" to BuildConfig.VERSION_NAME, "protocol" to 1, "sdk" to Build.VERSION.SDK_INT,
        "deviceName" to Settings.Global.getString(app.contentResolver, Settings.Global.DEVICE_NAME)?.trim()?.takeIf { it.isNotEmpty() },
        "accessibility" to (CompanionAccessibility.instance != null),
        "capabilities" to JSONObject().also { caps -> capabilities.forEach { caps.put(it, capability(it).json()) } })
}

/** UI integration surface. Mutating methods are local UI actions, never HTTP management endpoints. */
class CompanionController(private val context: Context) {
    private val app get() = CompanionRuntime.app
    fun termuxState(): TermuxState = try {
        app.packageManager.getApplicationInfo("com.termux", 0)
        TermuxState(true)
    } catch (_: PackageManager.NameNotFoundException) { TermuxState(false) }
    fun state(): CompanionState {
        val payload = JSONObject(app.assets.open("payload-state.json").bufferedReader().use { it.readText() })
        return CompanionState(BuildConfig.VERSION_NAME, 1, CompanionRuntime.server.port.takeIf { it > 0 },
            !payload.optBoolean("placeholder", true), termuxState(), CompanionRuntime.linked,
            CompanionAccessibility.instance != null, CompanionRuntime.capabilities.associateWith { CompanionRuntime.capability(it) },
            accessibilityEnabledInSystem = CompanionRuntime.accessibilityEnabledInSystem(),
            batteryExempt = CompanionRuntime.batteryExempt())
    }
    /** Opens the system dialog that lets the human exempt this app from battery optimization. */
    fun requestBatteryExemption() {
        context.startActivity(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:${app.packageName}")).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }
    fun start() { context.startForegroundService(Intent(context, HostService::class.java)) }
    fun stop() { CompanionRuntime.unlink(); context.stopService(Intent(context, HostService::class.java)) }
    fun openEmbeddedReview() = EmbeddedConsentActivity.launch(context, null)
    fun openEmbeddedPermission(name: String) = EmbeddedConsentActivity.launch(context, name)
    fun setCapability(name: String, enabled: Boolean) { CompanionRuntime.setEnabled(name, enabled) }
    fun regenerateLink() { require(termuxState().ready) { "Install Termux" }; CompanionRuntime.regenerate() }
    fun unlink() { CompanionRuntime.unlink() }
    fun installCommand(): String {
        require(termuxState().ready) { "Install Termux" }
        val manifest = JSONObject(app.assets.open("manifest.json").bufferedReader().use { it.readText() })
        val payload = JSONObject(app.assets.open("payload-state.json").bufferedReader().use { it.readText() })
        check(!payload.optBoolean("placeholder", true)) { "PLACEHOLDER APK: Agent assets absent; installation disabled" }
        val port = CompanionRuntime.server.port; check(port > 0) { "Start companion listener first" }
        val secret = CompanionRuntime.secret() ?: run { regenerateLink(); CompanionRuntime.secret()!! }
        return InstallCommand.create(port, manifest.getString("installSha256"), java.util.Base64.getUrlEncoder().withoutPadding().encodeToString(secret))
    }
    fun copyInstallCommand() {
        val clip = ClipData.newPlainText("Maxa Device install (sensitive)", installCommand())
        clip.description.extras = PersistableBundle().apply { putBoolean("android.content.extra.IS_SENSITIVE", true) }
        context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clip)
    }
    fun openSettings(kind: String) {
        val action = when (kind) {
            "ui" -> Settings.ACTION_ACCESSIBILITY_SETTINGS
            "install" -> Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES
            "orientation" -> Settings.ACTION_MANAGE_WRITE_SETTINGS
            "usage" -> Settings.ACTION_USAGE_ACCESS_SETTINGS
            "battery" -> Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS
            "app" -> Settings.ACTION_APPLICATION_DETAILS_SETTINGS
            else -> throw IllegalArgumentException("Unknown settings page")
        }
        val intent = Intent(action).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        if (kind in listOf("install", "orientation", "app")) intent.data = Uri.parse("package:${app.packageName}")
        context.startActivity(intent)
    }
    fun openTermuxDownload(channel: String) {
        val url = when (channel) {
            "fdroid" -> "https://f-droid.org/packages/com.termux/"
            "github" -> "https://github.com/termux/termux-app/releases"
            else -> throw IllegalArgumentException("Unknown channel")
        }
        context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }
}

class HostService : Service() {
    override fun onCreate() {
        super.onCreate(); startForeground(10, foregroundNotification(this, LISTENER_TEXT))
        CompanionRuntime.server.start()
    }
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int) = START_NOT_STICKY
    override fun onDestroy() { CompanionRuntime.server.stop(); super.onDestroy() }
    override fun onBind(intent: Intent?) = null
}
const val LISTENER_TEXT = "Termux companion listener"
const val CONTROL_TEXT = "Remote control active · keeping screen on"
fun foregroundNotification(context: Context, text: String): Notification {
    val manager = context.getSystemService(NotificationManager::class.java)
    manager.createNotificationChannel(NotificationChannel("companion", "Maxa Device", NotificationManager.IMPORTANCE_LOW))
    val open = PendingIntent.getActivity(context, 0, Intent(context, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
    return Notification.Builder(context, "companion").setSmallIcon(android.R.drawable.stat_sys_upload).setContentTitle("Maxa Device Companion").setContentText(text).setContentIntent(open).setOngoing(true).build()
}

