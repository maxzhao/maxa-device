package com.maxa.device.companion

import android.app.usage.UsageStatsManager
import android.content.*
import android.graphics.Point
import android.hardware.display.DisplayManager
import android.net.Uri
import android.os.SystemClock
import android.provider.Settings
import android.view.Display
import android.view.Surface
import org.json.JSONArray
import org.json.JSONObject

class Operations(private val context: Context) {
    val recording = RecordingManager(context)
    val installation = InstallationManager(context)
    val embeddedApi = EmbeddedApi(context)
    private fun ui(token: CallToken): CompanionAccessibility {
        CompanionRuntime.gate("ui", token)
        return CompanionAccessibility.instance ?: fail("ACCESSIBILITY_DISABLED", "Enable accessibility manually")
    }
    fun dispatch(op: String, args: JSONObject, token: CallToken, raw: ByteArray?): JSONObject {
        token.check()
        if (EmbeddedConsent.protectedSurface && (op.startsWith("ui.") || op.startsWith("apps.") ||
                op.startsWith("record.") || op.startsWith("clipboard.") || op == "url.open"))
            fail("HUMAN_CONFIRMATION_REQUIRED", "Local Android authorization in progress")
        if ((op.startsWith("ui.") || op in setOf("apps.launch", "url.open", "orientation.set")) && CompanionRuntime.capability("ui").ready)
            CompanionAccessibility.instance?.keepAwake()
        return when (op) {
            "status" -> CompanionRuntime.status()
            "api.call" -> embeddedApi.call(args, token, raw)
            "link.revoke" -> obj() // Server signs and flushes this reply before invalidating the link.
            "cancel" -> {
                val id = args.getString("requestId")
                CompanionRuntime.calls["${token.owner}:$id"]?.cancelled?.set(true); obj()
            }
            "ui.screen" -> ui(token).screen()
            "ui.foreground" -> ui(token).foreground()
            "ui.hierarchy" -> ui(token).hierarchy()
            "ui.screenshot" -> ui(token).screenshot(token)
            "ui.type" -> ui(token).type(args.getString("text"), args.optBoolean("submit", false), token)
            "ui.button" -> ui(token).button(args.getString("button"), token)
            "ui.tap", "ui.double_tap", "ui.long_press" -> {
                val service = ui(token); val x = args.getDouble("x").toFloat(); val y = args.getDouble("y").toFloat()
                val duration = if (op == "ui.long_press") args.getLong("durationMs") else 50L
                service.gesture(x, y, x, y, duration, token)
                if (op == "ui.double_tap") { Thread.sleep(75); token.check(); service.gesture(x, y, x, y, 50, token) } else obj()
            }
            "ui.swipe" -> ui(token).gesture(args.getDouble("x1").toFloat(), args.getDouble("y1").toFloat(), args.getDouble("x2").toFloat(), args.getDouble("y2").toFloat(), args.getLong("durationMs"), token)
            "apps.list" -> {
                ui(token)
                val intent = Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER)
                val apps = context.packageManager.queryIntentActivities(intent, 0).distinctBy { it.activityInfo.packageName }.sortedBy { it.activityInfo.packageName }
                obj("apps" to JSONArray().also { array -> apps.forEach { array.put(obj("name" to it.loadLabel(context.packageManager).toString(), "packageName" to it.activityInfo.packageName)) } })
            }
            "apps.launch" -> {
                ui(token)
                val intent = context.packageManager.getLaunchIntentForPackage(args.getString("packageName")) ?: fail("NOT_FOUND", "Launchable application not found")
                MainThread.call { CompanionRuntime.gate("ui", token); context.startActivity(intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) }; obj()
            }
            "url.open" -> {
                ui(token); val uri = Uri.parse(args.getString("url"))
                if (uri.scheme !in setOf("http", "https") || uri.host.isNullOrBlank()) fail("INVALID_ARGUMENT", "Only absolute http/https URLs supported")
                MainThread.call { CompanionRuntime.gate("ui", token); context.startActivity(Intent(Intent.ACTION_VIEW, uri).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) }; obj()
            }
            "apps.install" -> installation.install(args.getString("blobId"), token)
            "apps.uninstall" -> installation.uninstall(args.getString("packageName"), token)
            "record.start" -> recording.start(args, token)
            "record.stop" -> { CompanionRuntime.gate("recording", token); recording.stop(args.getString("recordingId"), token) }
            "orientation.set" -> orientation(args.getString("orientation"), token)
            "clipboard.get" -> { CompanionRuntime.gate("clipboard", token); obj("text" to ClipboardBridge.access(null, token)) }
            "clipboard.set" -> { CompanionRuntime.gate("clipboard", token); ClipboardBridge.access(args.getString("text"), token); obj() }
            "usage.query" -> {
                CompanionRuntime.gate("usage", token)
                val since = args.getLong("sinceMs"); val until = args.getLong("untilMs")
                if (since < 0 || until < since) fail("INVALID_ARGUMENT", "Invalid usage interval")
                val items = context.getSystemService(UsageStatsManager::class.java).queryAndAggregateUsageStats(since, until)
                // Only apps actually used in the interval, most foreground time first.
                val used = items.values.filter { it.totalTimeInForeground > 0 }.sortedByDescending { it.totalTimeInForeground }
                obj("items" to JSONArray().also { array -> used.forEach { value -> array.put(obj("packageName" to value.packageName, "foregroundMs" to value.totalTimeInForeground, "lastUsedMs" to value.lastTimeUsed)) } })
            }
            "blob.create" -> {
                CompanionRuntime.gate("install", token)
                val b = CompanionRuntime.blobs.create(token.owner, "install", args.getLong("size")); obj("blobId" to b.id)
            }
            "blob.write" -> {
                CompanionRuntime.gate("install", token)
                CompanionRuntime.blobs.write(token.owner, args.getString("blobId"), args.getLong("offset"), raw ?: fail("INVALID_ARGUMENT", "Raw body required")); obj()
            }
            "blob.delete" -> {
                val b = CompanionRuntime.blobs.get(args.getString("blobId"), token.owner); CompanionRuntime.blobs.remove(b.id); obj()
            }
            else -> fail("UNSUPPORTED", "Unknown companion operation: $op")
        }
    }
    private fun orientation(value: String, token: CallToken): JSONObject {
        if (value !in setOf("portrait", "landscape")) fail("INVALID_ARGUMENT", "orientation must be portrait or landscape")
        CompanionRuntime.gate("orientation", token)
        val portrait = value == "portrait"
        val target = MainThread.call {
            val display = context.getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY)
            val size = Point(); display.getRealSize(size)
            // Natural (rotation 0) orientation differs between phones (portrait) and many tablets (landscape).
            val quarterTurned = display.rotation == Surface.ROTATION_90 || display.rotation == Surface.ROTATION_270
            val naturalPortrait = if (quarterTurned) size.x > size.y else size.x < size.y
            val rotation = if (naturalPortrait == portrait) Surface.ROTATION_0 else Surface.ROTATION_90
            CompanionRuntime.gate("orientation", token)
            if (!Settings.System.putInt(context.contentResolver, Settings.System.ACCELEROMETER_ROTATION, 0) || !Settings.System.putInt(context.contentResolver, Settings.System.USER_ROTATION, rotation)) fail("OUTCOME_UNKNOWN", "Orientation settings not acknowledged")
            rotation
        }
        val deadline = SystemClock.elapsedRealtime() + 3000
        do {
            token.check()
            val (rotation, width, height) = MainThread.call {
                val display = context.getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY)
                val size = Point(); display.getRealSize(size); Triple(display.rotation, size.x, size.y)
            }
            if (rotation == target && (height > width) == portrait && Settings.System.getInt(context.contentResolver, Settings.System.ACCELEROMETER_ROTATION, -1) == 0 && Settings.System.getInt(context.contentResolver, Settings.System.USER_ROTATION, -1) == target)
                return obj("rotation" to rotation)
            Thread.sleep(100)
        } while (SystemClock.elapsedRealtime() < deadline)
        fail("OUTCOME_UNKNOWN", "Orientation read-back did not match; do not replay")
    }
}
