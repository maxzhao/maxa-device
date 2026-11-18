// SPDX-License-Identifier: GPL-3.0-only
// Maxa: restricted ports of Termux:API fc26ce1; no Termux receivers, streaming callbacks or command execution.
package com.maxa.device.companion

import android.Manifest
import android.app.Activity
import android.app.Notification
import android.app.NotificationManager
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.database.Cursor
import android.location.LocationManager
import android.net.Uri
import android.net.wifi.WifiManager
import android.os.SystemClock
import android.provider.CallLog
import android.provider.ContactsContract
import android.provider.Telephony
import android.service.notification.NotificationListenerService
import android.telephony.TelephonyManager
import org.json.JSONArray
import org.json.JSONObject

class EmbeddedNotificationListener : NotificationListenerService() {
    companion object { @Volatile internal var connected: EmbeddedNotificationListener? = null }
    override fun onListenerConnected() { connected = this }
    override fun onListenerDisconnected() { if (connected === this) connected = null }
    override fun onDestroy() { if (connected === this) connected = null; super.onDestroy() }
}

internal object EmbeddedProtected {
    val names = EmbeddedFiles.names + EmbeddedCapture.names + EmbeddedLocalEffects.names + setOf("location", "wifi-connectioninfo",
        "notification-list", "dialog", "share", "contact-list", "call-log", "sms-list", "sms-send", "telephony-call",
        "telephony-cellinfo")
    fun permissions(name: String): Array<String> = when (name) {
        "location", "wifi-connectioninfo", "telephony-cellinfo" -> arrayOf(Manifest.permission.ACCESS_FINE_LOCATION, Manifest.permission.ACCESS_COARSE_LOCATION)
        "contact-list" -> arrayOf(Manifest.permission.READ_CONTACTS)
        "call-log" -> arrayOf(Manifest.permission.READ_CALL_LOG)
        "sms-list" -> arrayOf(Manifest.permission.READ_SMS)
        "microphone-record", "speech-to-text" -> arrayOf(Manifest.permission.RECORD_AUDIO)
        else -> emptyArray()
    }
    fun missing(context: Context, name: String): String? {
        if (name == "job-scheduler" && !context.getSystemService(NotificationManager::class.java).areNotificationsEnabled())
            return "Allow APK notifications locally to schedule a reminder"
        if (name == "notification-list") {
            if (!context.getSystemService(NotificationManager::class.java).isNotificationListenerAccessGranted(ComponentName(context, EmbeddedNotificationListener::class.java)))
                return "Grant notification access locally; publishing notifications is separate"
            return if (EmbeddedNotificationListener.connected == null) "Notification listener is not connected" else null
        }
        if (permissions(name).filterNot { it == Manifest.permission.ACCESS_COARSE_LOCATION }.any { context.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED })
            return if (name == "call-log" || name == "sms-list") "Android did not grant the restricted permission (installer/role controlled)" else "Grant Android permission locally first"
        return null
    }
    private fun phone(args: JSONObject): String = args.text("number", 16).also {
        if (!Regex("\\+?[0-9]{3,15}").matches(it)) fail("INVALID_ARGUMENT", "Use a single telephone number (no USSD, separators or pauses)")
    }
    private fun display(args: JSONObject, key: String, max: Int): String = args.text(key, max).also {
        if (it.any { c -> c.isISOControl() && c != '\n' || Character.getType(c) == Character.FORMAT.toInt() })
            fail("INVALID_ARGUMENT", "Text must not contain hidden control characters")
    }
    fun describe(name: String, args: JSONObject): String {
        if (name in EmbeddedFiles.names) return EmbeddedFiles.describe(name, args)
        if (name in EmbeddedCapture.names) return EmbeddedCapture.describe(name, args)
        if (name in EmbeddedLocalEffects.names) return EmbeddedLocalEffects.describe(name, args)
        val action = when (name) {
            "sms-send" -> { args.only("number", "text"); "Open the system SMS composer. You must send it yourself.\n\nTo: ${phone(args)}\n\n${display(args, "text", 500)}" }
            "telephony-call" -> { args.only("number"); "Open the system dialer for ${phone(args)}. You must press Call yourself." }
            "dialog" -> { args.only("title", "text"); "Show a local confirmation dialog: ${display(args,"title",100)}\n${display(args,"text",500)}" }
            "share" -> { args.only("text"); "Share this text with an app you select:\n\n${display(args,"text",1000)}" }
            "contact-list", "call-log", "sms-list", "notification-list" -> {
                args.only("limit"); "Disclose up to ${limit(args)} entries from $name to linked Termux software, once."
            }
            else -> { args.only(); "Disclose one $name snapshot to linked Termux software. No background collection." }
        }
        return "Requested by linked Termux software:\n\n$action"
    }
    private fun limit(args: JSONObject) = if (args.has("limit")) args.int("limit", 1..50) else 20
    fun execute(a: EmbeddedConsentActivity, e: EmbeddedConsent.Request) {
        EmbeddedConsent.checkLive(e)
        val args = e.args
        when {
            e.name in EmbeddedFiles.names -> EmbeddedFiles.execute(a, e)
            e.name in EmbeddedCapture.names -> EmbeddedCapture.execute(a, e)
            e.name in EmbeddedLocalEffects.names -> EmbeddedLocalEffects.execute(a, e)
            e.name == "dialog" -> a.confirm(e, display(args, "title", 100), display(args, "text", 500))
            e.name in setOf("sms-send", "telephony-call", "share") -> {
                val intent = when (e.name) {
                    "sms-send" -> {
                        val pkg = Telephony.Sms.getDefaultSmsPackage(a) ?: fail("PLATFORM_RESTRICTED", "No default SMS app")
                        Intent(Intent.ACTION_SENDTO, Uri.fromParts("smsto", phone(args), null)).setPackage(pkg).putExtra("sms_body", display(args,"text",500))
                    }
                    "telephony-call" -> {
                        val pkg = a.getSystemService(android.telecom.TelecomManager::class.java)?.defaultDialerPackage
                            ?: fail("PLATFORM_RESTRICTED", "No default dialer")
                        Intent(Intent.ACTION_DIAL, Uri.fromParts("tel", phone(args), null)).setPackage(pkg)
                    }
                    else -> Intent.createChooser(Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, display(args,"text",1000)), null)
                }
                a.openSystem(intent) { _, _ -> a.finished(e, Result.success(obj("handedOff" to true, "effectVerified" to false))) }
            }
            else -> EmbeddedConsent.work(a, e) { snapshot(a, e.name, args) }
        }
    }
    private fun query(context: Context, uri: Uri, columns: Array<String>, order: String?, limit: Int, row: (Cursor) -> JSONObject): JSONArray =
        context.contentResolver.query(uri, columns, null, null, order)?.use { c ->
            JSONArray().apply { while (length() < limit && c.moveToNext()) put(row(c)) }
        } ?: fail("DEVICE_UNAVAILABLE", "Android provider did not return data")
    private fun snapshot(ctx: Context, name: String, args: JSONObject): Any = when (name) {
        "location" -> {
            val lm = ctx.getSystemService(LocationManager::class.java) ?: fail("NO_HARDWARE", "No location service")
            if (!lm.isLocationEnabled) fail("DEVICE_UNAVAILABLE", "Location is disabled in Android settings")
            val loc = lm.getProviders(true).mapNotNull { lm.getLastKnownLocation(it) }.maxByOrNull { it.elapsedRealtimeNanos }
                ?: fail("NOT_FOUND", "No cached location; no tracking was started")
            obj("latitude" to loc.latitude, "longitude" to loc.longitude, "altitude" to loc.altitude,
                "accuracy" to loc.accuracy, "bearing" to loc.bearing, "speed" to loc.speed, "provider" to loc.provider,
                "elapsedMs" to (SystemClock.elapsedRealtimeNanos() - loc.elapsedRealtimeNanos) / 1_000_000)
        }
        "wifi-connectioninfo" -> {
            if (!ctx.packageManager.hasSystemFeature(PackageManager.FEATURE_WIFI)) fail("NO_HARDWARE", "No Wi-Fi support")
            if (!ctx.getSystemService(LocationManager::class.java).isLocationEnabled) fail("DEVICE_UNAVAILABLE", "Enable Android location locally")
            val wm = ctx.applicationContext.getSystemService(WifiManager::class.java)
            if (!wm.isWifiEnabled) fail("DEVICE_UNAVAILABLE", "Wi-Fi is off")
            wm.connectionInfo.let {
                if (it == null || it.bssid == null || it.bssid == "02:00:00:00:00:00" || it.ssid == "<unknown ssid>")
                    fail("DEVICE_UNAVAILABLE", "No current connection or identifiers are redacted by Android")
                obj("ssid" to it.ssid.trim('"'), "bssid" to it.bssid, "frequency_mhz" to it.frequency,
                    "link_speed_mbps" to it.linkSpeed, "rssi" to it.rssi)
            }
        }
        "notification-list" -> JSONArray().apply {
            val svc = EmbeddedNotificationListener.connected ?: fail("DEVICE_UNAVAILABLE", "Notification listener not connected")
            svc.activeNotifications.orEmpty().filter { it.packageName != ctx.packageName && it.notification.visibility != Notification.VISIBILITY_SECRET }.take(limit(args)).forEach {
                val x = it.notification.extras
                put(obj("id" to it.id, "packageName" to it.packageName, "title" to x.getCharSequence(Notification.EXTRA_TITLE)?.toString()?.take(160),
                    "text" to (x.getCharSequence(Notification.EXTRA_BIG_TEXT) ?: x.getCharSequence(Notification.EXTRA_TEXT))?.toString()?.take(500)))
            }
        }
        "contact-list" -> {
            val cols = arrayOf(ContactsContract.CommonDataKinds.Phone.DISPLAY_NAME, ContactsContract.CommonDataKinds.Phone.NUMBER)
            query(ctx, ContactsContract.CommonDataKinds.Phone.CONTENT_URI, cols, cols[0], limit(args)) {
                obj("name" to it.getString(0)?.take(160), "number" to it.getString(1)?.take(64))
            }
        }
        "call-log" -> {
            val cols = arrayOf(CallLog.Calls.CACHED_NAME, CallLog.Calls.NUMBER, CallLog.Calls.TYPE, CallLog.Calls.DATE, CallLog.Calls.DURATION)
            query(ctx, CallLog.Calls.CONTENT_URI, cols, "date DESC", limit(args)) {
                obj("name" to it.getString(0)?.take(160), "phone_number" to it.getString(1)?.take(64), "type" to it.getInt(2),
                    "dateMs" to it.getLong(3), "durationSeconds" to it.getLong(4))
            }
        }
        "sms-list" -> {
            val cols = arrayOf(Telephony.Sms.ADDRESS, Telephony.Sms.BODY, Telephony.Sms.DATE, Telephony.Sms.TYPE)
            query(ctx, Telephony.Sms.CONTENT_URI, cols, "date DESC", limit(args)) {
                obj("address" to it.getString(0)?.take(64), "body" to it.getString(1)?.take(1200), "dateMs" to it.getLong(2), "type" to it.getInt(3))
            }
        }
        "telephony-cellinfo" -> {
            if (!ctx.packageManager.hasSystemFeature(PackageManager.FEATURE_TELEPHONY)) fail("NO_HARDWARE", "No cellular telephony")
            val tm = ctx.getSystemService(TelephonyManager::class.java)
            val cells = tm.allCellInfo ?: fail("DEVICE_UNAVAILABLE", "No cached cellular snapshot")
            JSONArray().apply { cells.take(32).forEach {
                val (identity, dbm) = when (it) {
                    is android.telephony.CellInfoLte -> it.cellIdentity.ci to it.cellSignalStrength.dbm
                    is android.telephony.CellInfoNr -> (it.cellIdentity as android.telephony.CellIdentityNr).nci to it.cellSignalStrength.dbm
                    is android.telephony.CellInfoGsm -> it.cellIdentity.cid to it.cellSignalStrength.dbm
                    is android.telephony.CellInfoWcdma -> it.cellIdentity.cid to it.cellSignalStrength.dbm
                    else -> null to null
                }
                put(obj("type" to it.javaClass.simpleName, "registered" to it.isRegistered, "identity" to identity, "dbm" to dbm))
            } }
        }
        else -> error("Unmapped embedded snapshot")
    }
}
