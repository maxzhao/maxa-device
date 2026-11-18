// SPDX-License-Identifier: GPL-3.0-only
// Maxa: picker-based adaptations of StorageGetAPI and WallpaperAPI from Termux:API fc26ce1.
package com.maxa.device.companion

import android.app.Activity
import android.app.WallpaperManager
import android.content.Intent
import android.provider.DocumentsContract as Docs
import android.util.Base64
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.io.InputStream

internal object EmbeddedFiles {
    val names = setOf("wallpaper", "storage-get")
    const val LIMIT = 256 * 1024

    fun describe(name: String, args: JSONObject): String {
        args.only()
        return "Requested by linked Termux software: " + when (name) {
            "wallpaper" -> "Set the system wallpaper from a local image you select. No image is returned."
            "storage-get" -> "Return the bytes of one document you select to linked Termux software (max ${LIMIT / 1024} KiB)."
            else -> error("Unmapped embedded file request")
        }
    }

    fun bytes(input: InputStream, max: Int): ByteArray = input.use {
        val out = ByteArrayOutputStream()
        val chunk = ByteArray(8192)
        while (true) {
            val count = it.read(chunk, 0, minOf(chunk.size, max + 1 - out.size()))
            if (count == -1) break
            if (count == 0) fail("DEVICE_UNAVAILABLE", "Provider did not make progress reading the selected data")
            out.write(chunk, 0, count)
            if (out.size() > max) fail("DATA_LIMIT", "Selected data exceeds the size limit")
        }
        out.toByteArray()
    }

    fun encoded(data: ByteArray, type: String?) = obj(
        "base64" to Base64.encodeToString(data, Base64.NO_WRAP), "size" to data.size, "mime" to type)

    fun execute(a: EmbeddedConsentActivity, e: EmbeddedConsent.Request) {
        val picker = Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE)
            .setType(if (e.name == "wallpaper") "image/*" else "*/*")
            .putExtra(Intent.EXTRA_LOCAL_ONLY, true)
        a.openSystem(picker) { result, data ->
            if (result != Activity.RESULT_OK) fail("CANCELLED", "No document selected")
            val uri = data?.data ?: fail("NOT_FOUND", "The picker did not return a document")
            if (uri.scheme != "content" || !Docs.isDocumentUri(a, uri) || uri.authority == a.packageName)
                fail("INVALID_ARGUMENT", "Picker did not return a supported document URI")
            val resolver = a.contentResolver
            EmbeddedConsent.work(a, e) {
                when (e.name) {
                    "wallpaper" -> {
                        val wm = WallpaperManager.getInstance(a)
                        if (!wm.isWallpaperSupported || !wm.isSetWallpaperAllowed) fail("PLATFORM_RESTRICTED", "This user cannot set wallpaper")
                        if (resolver.getType(uri)?.startsWith("image/") != true) fail("INVALID_ARGUMENT", "Select an image")
                        val payload = bytes(resolver.openInputStream(uri) ?: fail("NOT_FOUND", "Image unavailable"), 4 * 1024 * 1024)
                        EmbeddedConsent.checkLive(e)
                        val id = wm.setStream(payload.inputStream(), null, false, WallpaperManager.FLAG_SYSTEM)
                        if (id == 0) fail("OUTCOME_UNKNOWN", "Android did not confirm wallpaper change; do not replay")
                        obj("applied" to true)
                    }
                    "storage-get" -> encoded(bytes(resolver.openInputStream(uri)
                        ?: fail("NOT_FOUND", "Selected document unavailable"), LIMIT), resolver.getType(uri))
                    else -> error("Unmapped embedded file request")
                }
            }
        }
    }
}
