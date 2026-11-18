package com.maxa.device.companion

import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.net.Uri
import android.provider.DocumentsContract
import android.util.Log
import org.json.JSONObject

/** Startup cleanup only. The legacy registry is a retry journal, not an authorization source. */
internal object RetiredEmbeddedApiMigration {
    private val oldId = Regex("[0-9a-f]{32}")
    private val retiredNames = listOf(
        "wifi-enable", "wifi-scaninfo", "keystore", "telephony-deviceinfo", "media-scan", "download",
        "saf-managedir", "saf-dirs", "saf-ls", "saf-stat", "saf-read", "saf-create", "saf-write", "saf-mkdir", "saf-rm"
    )

    fun run(context: Context, capabilityPrefs: SharedPreferences) {
        // Never enumerate or clear the main preference store; it also contains the link credential.
        val removed = runCatching {
            val switches = capabilityPrefs.edit()
            retiredNames.forEach { switches.remove("cap.api.$it") }
            switches.commit()
        }.getOrDefault(false)
        if (!removed) Log.w("EmbeddedApiMigration", "Retired switch cleanup will retry at startup")

        try {
            val registry = context.getSharedPreferences("embedded-saf-registry", Context.MODE_PRIVATE)
            val resolver = context.contentResolver
            var pending = false
            for ((id, value) in registry.all) {
                val uri = legacyDirectory(context, id, value)
                if (uri == null || !retireRecordedDirectoryReadGrant(
                        hasPersistedRead = { resolver.persistedUriPermissions.any { it.uri == uri && it.isReadPermission } },
                        releaseRead = { resolver.releasePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION) }
                    )) {
                    pending = true
                    continue
                }
                // Revoke first; only now may we forget the record. A crash before commit can safely retry.
                if (!registry.edit().remove(id).commit()) pending = true
            }
            if (pending) Log.w("EmbeddedApiMigration", "Legacy directory cleanup still pending; registry not fully cleared")
        } catch (_: RuntimeException) {
            // No exception object or row data: provider messages can contain a private URI.
            Log.w("EmbeddedApiMigration", "Legacy directory cleanup could not finish; retry at next startup")
        }
    }

    private fun legacyDirectory(context: Context, id: String, value: Any?): Uri? {
        if (!oldId.matches(id) || value !is String) return null
        return try {
            val record = JSONObject(value)
            val raw = record.opt("uri") as? String ?: return null
            if (record.opt("name") !is String) return null
            Uri.parse(raw).takeIf {
                it.scheme == "content" && !it.authority.isNullOrBlank() && it.authority != context.packageName &&
                    DocumentsContract.isTreeUri(it) && it.pathSegments.size == 2 && it.pathSegments[1].isNotBlank()
            }
        } catch (_: Exception) { null }
    }
}

/** Forget only when Android confirms the exact persisted read mode is absent. */
internal fun retireRecordedDirectoryReadGrant(hasPersistedRead: () -> Boolean, releaseRead: () -> Unit): Boolean {
    return try {
        if (!hasPersistedRead()) true
        else {
            try { releaseRead() } catch (_: RuntimeException) { /* A concurrent revoke can still have succeeded. */ }
            !hasPersistedRead()
        }
    } catch (_: RuntimeException) { false }
}
