package com.maxa.device.companion

import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.hardware.ConsumerIrManager
import android.hardware.camera2.CameraAccessException
import android.hardware.camera2.CameraManager
import android.media.AudioManager
import android.os.Handler
import android.os.Looper
import android.os.Vibrator
import android.provider.Settings
import com.termux.api.apis.*
import com.termux.api.util.ResultReturner
import org.json.JSONArray
import org.json.JSONObject
import org.json.JSONTokener

/** Authenticated JSON RPC, not a Termux Intent or descriptor ABI. Registry is also the local settings surface. */
class EmbeddedApi(private val context: Context) {
    companion object {
        val implementedNames: Set<String> = setOf(
            "battery-status", "brightness", "volume", "audio-info", "vibrate", "torch", "sensor",
            "notification", "notification-channel", "notification-remove", "toast", "tts-engines", "camera-info",
            "infrared-frequencies", "infrared-transmit", "usb"
        ) + EmbeddedProtected.names
        val capabilityIds: List<String> = implementedNames.map { "api.$it" }
        internal fun capability(id: String, context: Context, enabled: Boolean): CapabilityState? {
            if (id !in capabilityIds) return null
            val missing = when (id) {
                "api.brightness" -> if (!Settings.System.canWrite(context)) "Grant Modify system settings manually" else null
                "api.notification", "api.notification-channel" ->
                    if (!context.getSystemService(NotificationManager::class.java).areNotificationsEnabled()) "Allow this app's notifications manually" else null
                else -> EmbeddedProtected.missing(context, id.removePrefix("api."))
            }
            return CapabilityState(enabled, missing == null, enabled && missing == null,
                if (!enabled) "Capability disabled in companion" else missing)
        }
    }

    private val readOnly = setOf("battery-status", "audio-info", "camera-info", "sensor", "tts-engines", "infrared-frequencies", "usb")
    private val torchHandler = Handler(Looper.getMainLooper())
    private var litTorch: String? = null // main thread only
    private val stopTorch = Runnable {
        litTorch?.let { id ->
            try { context.getSystemService(CameraManager::class.java).setTorchMode(id, false); litTorch = null }
            catch (_: CameraAccessException) { /* Android unavailable; never turn it back on automatically. */ }
            catch (_: IllegalArgumentException) { litTorch = null }
            catch (_: SecurityException) { /* Still try again if locally revoked or unlinked. */ }
        }
    }
    fun invalidate() { torchHandler.post(stopTorch) }

    fun call(request: JSONObject, token: CallToken, raw: ByteArray?): JSONObject {
        request.only("name", "args")
        if (raw != null) fail("INVALID_ARGUMENT", "api.call accepts JSON only")
        val name = request.text("name", 64)
        val args = request.opt("args") as? JSONObject ?: fail("INVALID_ARGUMENT", "args must be an object")
        token.check()
        if (name !in implementedNames) fail("INVALID_ARGUMENT", "Unknown embedded API name")
        val guard = Runnable { CompanionRuntime.gate("api.$name", token) }
        guard.run()
        if (name in EmbeddedProtected.names) return EmbeddedConsent.call(name, args, token)
        if (EmbeddedConsent.protectedSurface && name !in readOnly)
            fail("HUMAN_CONFIRMATION_REQUIRED", "Local Android authorization in progress")
        val result = when (name) {
            "battery-status" -> read(args, guard) { BatteryStatusAPI.onReceive(null, context, it) }
            "audio-info" -> read(args, guard) { AudioAPI.onReceive(null, context, it) }
            "camera-info" -> {
                args.only()
                if (context.getSystemService(CameraManager::class.java)?.cameraIdList.isNullOrEmpty()) fail("NO_HARDWARE", "No camera visible to this APK")
                capture(guard, Intent()) { CameraInfoAPI.onReceive(null, context, it) }
            }
            "brightness" -> {
                args.only("brightness", "auto")
                val auto = args.bool("auto", false)
                if (auto && args.has("brightness")) fail("INVALID_ARGUMENT", "Choose automatic mode or brightness, not both")
                val input = Intent().putExtra("auto", auto)
                if (!auto) input.putExtra("brightness", args.int("brightness", 0..255))
                capture(guard, input) { BrightnessAPI.onReceive(null, context, it) }
            }
            "volume" -> {
                args.only("stream", "volume")
                val input = Intent()
                if (args.length() != 0) {
                    // Ring/notification/system changes can affect Do Not Disturb. Do not permit them here.
                    val stream = args.text("stream", 16)
                    val androidStream = when (stream) {
                        "music" -> AudioManager.STREAM_MUSIC
                        "alarm" -> AudioManager.STREAM_ALARM
                        else -> fail("INVALID_ARGUMENT", "Writable streams: music, alarm")
                    }
                    val audio = context.getSystemService(AudioManager::class.java)
                    if (audio.isVolumeFixed) fail("PLATFORM_RESTRICTED", "Android fixed-volume policy prevents volume changes")
                    input.action = "set-volume"
                    input.putExtra("stream", stream).putExtra("volume", args.int("volume",
                        audio.getStreamMinVolume(androidStream)..audio.getStreamMaxVolume(androidStream)))
                }
                capture(guard, input) { VolumeAPI.onReceive(null, context, it) }
            }
            "vibrate" -> {
                args.only("duration_ms")
                val intent = Intent().putExtra("duration_ms", if (args.has("duration_ms")) args.int("duration_ms", 1..3000) else 1000)
                if (context.getSystemService(Vibrator::class.java)?.hasVibrator() != true) fail("NO_HARDWARE", "No vibrator")
                capture(guard, intent) { VibrateAPI.onReceive(null, context, it) }
            }
            "torch" -> torch(args, guard)
            "infrared-frequencies", "infrared-transmit" -> infrared(name, args, guard)
            else -> EmbeddedImmediate.call(context, name, args, guard)
        }
        // Do not turn an acknowledged effect into a misleading no-effect error. Read results
        // still get a final authorization check before sending data.
        if (name in readOnly || name == "volume" && args.length() == 0) guard.run()
        return obj("value" to result)
    }

    private fun read(args: JSONObject, guard: Runnable, run: (Intent) -> Unit): Any {
        args.only()
        return capture(guard, Intent(), run)
    }

    private fun capture(guard: Runnable, input: Intent, run: (Intent) -> Unit): Any {
        // input is constructed locally with fixed, typed extras and is never sent to Android.
        val output = ResultReturner.invoke(guard) { run(input) }
        if (output.isBlank()) return obj("requested" to true)
        val encoded = JSONTokener(output)
        val decoded = try { encoded.nextValue() } catch (_: org.json.JSONException) { fail("OUTCOME_UNKNOWN", "API returned invalid JSON; do not replay") }
        if (encoded.nextClean() != '\u0000' || decoded !is JSONObject && decoded !is JSONArray)
            fail("OUTCOME_UNKNOWN", "API returned invalid JSON; do not replay")
        if (decoded is JSONObject && decoded.has("API_ERROR")) fail("OPERATION_FAILED", "Android API did not supply a result")
        return decoded
    }

    private fun torch(args: JSONObject, guard: Runnable): JSONObject {
        args.only("enabled")
        val enabled = args.bool("enabled")
        return MainThread.call {
            guard.run()
            val manager = context.getSystemService(CameraManager::class.java) ?: fail("NO_HARDWARE", "No camera service")
            try {
                val id = if (enabled) litTorch ?: TorchAPI.getTorchCameraId(manager) ?: fail("NO_HARDWARE", "No rear flash")
                         else litTorch ?: return@call obj("enabled" to false, "owned" to false)
                guard.run()
                manager.setTorchMode(id, enabled)
                torchHandler.removeCallbacks(stopTorch)
                litTorch = if (enabled) id else null
                if (enabled) torchHandler.postDelayed(stopTorch, 30_000)
                obj("enabled" to enabled, "autoOffMs" to if (enabled) 30_000 else null)
            } catch (_: CameraAccessException) { fail("DEVICE_UNAVAILABLE", "Camera flash is currently unavailable") }
        }
    }

    private fun infrared(name: String, args: JSONObject, guard: Runnable): Any {
        val transmit = name == "infrared-transmit"
        if (transmit) args.only("frequency", "pattern") else args.only()
        val ir = context.getSystemService(ConsumerIrManager::class.java)
        if (ir?.hasIrEmitter() != true) fail("NO_HARDWARE", "No infrared emitter")
        if (!transmit) return capture(guard, Intent()) { InfraredAPI.onReceiveCarrierFrequency(null, context, it) }
        val frequency = args.int("frequency", 1..1_000_000)
        val input = args.opt("pattern") as? JSONArray ?: fail("INVALID_ARGUMENT", "pattern must be an array of positive microsecond durations")
        if (input.length() !in 1..512) fail("INVALID_ARGUMENT", "pattern length must be between 1 and 512")
        val pattern = IntArray(input.length()) { requireInt(input.opt(it), 1..250_000, "pattern entry") }
        if (pattern.sum() > 250_000) fail("INVALID_ARGUMENT", "Infrared transmission exceeds 250 ms")
        val bands = ir.carrierFrequencies ?: fail("DEVICE_UNAVAILABLE", "Carrier frequency ranges are unavailable")
        if (bands.none { frequency in it.minFrequency..it.maxFrequency }) fail("INVALID_ARGUMENT", "Carrier frequency is outside hardware ranges")
        return capture(guard, Intent().putExtra("frequency", frequency).putExtra("pattern", pattern)) {
            InfraredAPI.onReceiveTransmit(null, context, it)
        }
    }
}

internal fun JSONObject.only(vararg accepted: String) {
    if (keys().asSequence().any { it !in accepted }) fail("INVALID_ARGUMENT", "Unknown argument; operation not started")
}
internal fun JSONObject.text(key: String, max: Int): String = (opt(key) as? String)?.takeIf { it.isNotBlank() && it.length <= max }
    ?: fail("INVALID_ARGUMENT", "$key must be a nonblank string of at most $max characters")
internal fun JSONObject.bool(key: String, default: Boolean? = null): Boolean =
    if (!has(key) && default != null) default else opt(key) as? Boolean ?: fail("INVALID_ARGUMENT", "$key must be boolean")
internal fun requireInt(value: Any?, range: IntRange, label: String): Int {
    val number = when (value) { is Int -> value.toLong(); is Long -> value; else -> fail("INVALID_ARGUMENT", "$label must be an integer") }
    if (number !in range.first.toLong()..range.last.toLong()) fail("INVALID_ARGUMENT", "$label outside allowed range")
    return number.toInt()
}
internal fun JSONObject.int(key: String, range: IntRange) = requireInt(opt(key), range, key)
