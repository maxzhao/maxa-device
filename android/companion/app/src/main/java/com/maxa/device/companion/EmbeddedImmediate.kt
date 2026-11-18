// SPDX-License-Identifier: GPL-3.0-only
// Maxa (2026-11-18): restricted ports of Termux:API fc26ce1 Notification, Sensor, Toast, TextToSpeech and Usb API paths.
package com.maxa.device.companion

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.hardware.Sensor
import android.hardware.SensorManager
import android.hardware.usb.UsbManager
import android.speech.tts.TextToSpeech
import android.widget.Toast
import org.json.JSONArray
import org.json.JSONObject

internal object EmbeddedImmediate {
    private const val NAMESPACE = "maxa-api."
    private val safeId = Regex("[A-Za-z0-9_-]{1,40}")
    private fun identifier(raw: String): String {
        if (!safeId.matches(raw)) fail("INVALID_ARGUMENT", "Identifier must consist of letters, digits, underscores or hyphens")
        return NAMESPACE + raw
    }

    fun call(context: Context, name: String, args: JSONObject, guard: Runnable): Any = when (name) {
        "notification", "notification-remove", "notification-channel" -> notification(context, name, args, guard)
        "toast" -> {
            args.only("text", "short")
            val input = args.text("text", 160)
            val duration = if (args.bool("short", true)) Toast.LENGTH_SHORT else Toast.LENGTH_LONG
            MainThread.call {
                guard.run()
                Toast.makeText(context, input, duration).show()
                obj("requested" to true)
            }
        }
        "sensor" -> {
            args.only("list")
            if (!args.bool("list", true)) fail("INVALID_ARGUMENT", "Only sensor inventory is implemented; sampling is not enabled")
            val manager = context.getSystemService(SensorManager::class.java) ?: fail("NO_HARDWARE", "No sensor service")
            val names = manager.getSensorList(Sensor.TYPE_ALL).map { it.name }.distinct().sorted()
            if (names.isEmpty()) fail("NO_HARDWARE", "No sensors available")
            obj("sensors" to JSONArray(names))
        }
        "tts-engines" -> {
            args.only()
            // Discover services without starting or binding to a third-party speech engine.
            val pm = context.packageManager
            val engines = pm.queryIntentServices(Intent(TextToSpeech.Engine.INTENT_ACTION_TTS_SERVICE), PackageManager.MATCH_DEFAULT_ONLY)
                .distinctBy { it.serviceInfo.packageName }.sortedBy { it.serviceInfo.packageName }
            JSONArray().also { array ->
                engines.forEach { service -> array.put(obj("name" to service.serviceInfo.packageName, "label" to service.loadLabel(pm).toString())) }
            }
        }
        "usb" -> {
            args.only("list")
            if (!args.bool("list", true)) fail("INVALID_ARGUMENT", "Only USB inventory is implemented; open and descriptor transfer are not enabled")
            if (!context.packageManager.hasSystemFeature(PackageManager.FEATURE_USB_HOST)) fail("NO_HARDWARE", "No USB host feature")
            val manager = context.getSystemService(UsbManager::class.java) ?: fail("NO_HARDWARE", "No USB host service")
            JSONArray().also { array -> manager.deviceList.values.sortedBy { it.deviceId }.forEach {
                // Never return a /dev path, serial number, descriptor or file handle.
                array.put(obj("vendorId" to it.vendorId, "productId" to it.productId, "deviceClass" to it.deviceClass,
                    "deviceSubclass" to it.deviceSubclass, "deviceProtocol" to it.deviceProtocol))
            } }
        }
        else -> error("Unmapped embedded API registry entry")
    }

    private fun notification(context: Context, op: String, args: JSONObject, guard: Runnable): JSONObject {
        val manager = context.getSystemService(NotificationManager::class.java)
        return when (op) {
            "notification-channel" -> {
                args.only("id", "name", "delete")
                val id = identifier(args.text("id", 40))
                val delete = args.bool("delete", false)
                if (delete && args.has("name")) fail("INVALID_ARGUMENT", "A channel name cannot be used with delete")
                val label = if (!delete) args.text("name", 60) else null
                guard.run()
                if (delete) {
                    if (manager.getNotificationChannel(id) == null) fail("NOT_FOUND", "Embedded notification channel not found")
                    manager.deleteNotificationChannel(id)
                } else {
                    manager.createNotificationChannel(NotificationChannel(id, "Embedded API · $label", NotificationManager.IMPORTANCE_LOW))
                    if (manager.getNotificationChannel(id) == null) fail("OUTCOME_UNKNOWN", "Notification channel not acknowledged; do not replay")
                }
                obj("requested" to true)
            }
            "notification-remove" -> {
                args.only("id")
                val tag = identifier(args.text("id", 40))
                guard.run()
                manager.cancel(tag, 0)
                obj("requested" to true)
            }
            else -> {
                args.only("id", "title", "text", "channel")
                val tag = identifier(args.text("id", 40))
                val channel = identifier(args.text("channel", 40))
                val title = args.text("title", 100)
                val text = args.text("text", 1000)
                val existing = manager.getNotificationChannel(channel) ?: fail("NOT_FOUND", "Create this embedded notification channel first")
                if (existing.importance == NotificationManager.IMPORTANCE_NONE) fail("PERMISSION_MISSING", "The selected notification channel is blocked")
                guard.run()
                manager.notify(tag, 0, Notification.Builder(context, channel)
                    .setSmallIcon(android.R.drawable.ic_dialog_info).setContentTitle("Embedded API · $title")
                    .setContentText(text).setStyle(Notification.BigTextStyle().bigText(text))
                    .setVisibility(Notification.VISIBILITY_SECRET).setAutoCancel(true).build())
                obj("requested" to true)
            }
        }
    }
}
