// SPDX-License-Identifier: GPL-3.0-only
// Maxa: bounded ports of Termux:API fc26ce1 camera, microphone, speech, media, fingerprint and NFC paths.
package com.maxa.device.companion

import android.app.Activity
import android.content.Intent
import android.graphics.Bitmap
import android.hardware.biometrics.BiometricManager
import android.hardware.biometrics.BiometricPrompt
import android.media.MediaPlayer
import android.media.MediaRecorder
import android.nfc.NfcAdapter
import android.nfc.tech.Ndef
import android.os.Build
import android.os.Bundle
import android.os.CancellationSignal
import android.os.Handler
import android.os.Looper
import android.provider.MediaStore
import android.speech.RecognitionListener
import android.speech.RecognizerIntent
import android.speech.SpeechRecognizer
import org.json.JSONArray
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.io.File

internal object EmbeddedCapture {
    val names = setOf("fingerprint", "camera-photo", "microphone-record", "speech-to-text", "media-player", "nfc")
    private fun seconds(args: JSONObject, max: Int, default: Int) = if (args.has("seconds")) args.int("seconds", 1..max) else default
    fun describe(name: String, args: JSONObject): String = "Requested by linked Termux software:\n\n" + when (name) {
        "fingerprint" -> { args.only(); "Run Android strong biometric authentication. Only the outcome is returned." }
        "camera-photo" -> { args.only(); "Open the system camera. Take and accept a photo yourself to return its thumbnail." }
        "microphone-record" -> { args.only("seconds"); "Record up to ${seconds(args, 12, 6)} seconds of microphone audio and return it. You can stop or close this screen at any time." }
        "speech-to-text" -> { args.only(); "Listen for at most 15 seconds, using Android on-device recognition, and return the text." }
        "media-player" -> { args.only("seconds"); "Play a local audio file you select, for at most ${seconds(args, 60, 30)} seconds. No file is returned." }
        else -> { args.only(); "Read one nearby NFC tag while this screen is visible. Its NDEF data (if any) will be returned." }
    }
    fun execute(a: EmbeddedConsentActivity, e: EmbeddedConsent.Request) {
        when (e.name) {
            "fingerprint" -> biometric(a, e)
            "camera-photo" -> a.openSystem(Intent(MediaStore.ACTION_IMAGE_CAPTURE)) { code, data ->
                if (code != Activity.RESULT_OK) fail("CANCELLED", "No photo accepted")
                val bmp = data?.extras?.get("data") as? Bitmap ?: fail("DEVICE_UNAVAILABLE", "Camera did not return a thumbnail")
                if (bmp.width.toLong() * bmp.height > 720_000) fail("DATA_LIMIT", "Camera thumbnail is too large")
                val stream = ByteArrayOutputStream()
                if (!bmp.compress(Bitmap.CompressFormat.JPEG, 75, stream) || stream.size() > EmbeddedFiles.LIMIT)
                    fail("DATA_LIMIT", "Camera thumbnail could not fit the bounded output")
                a.finished(e, Result.success(EmbeddedFiles.encoded(stream.toByteArray(), "image/jpeg")))
            }
            "microphone-record" -> record(a, e)
            "speech-to-text" -> speech(a, e)
            "media-player" -> media(a, e)
            "nfc" -> nfc(a, e)
        }
    }
    private fun biometric(a: EmbeddedConsentActivity, e: EmbeddedConsent.Request) {
        val allowed = BiometricManager.Authenticators.BIOMETRIC_STRONG
        val manager = a.getSystemService(BiometricManager::class.java)
        when (manager.canAuthenticate(allowed)) {
            BiometricManager.BIOMETRIC_SUCCESS -> Unit
            BiometricManager.BIOMETRIC_ERROR_NO_HARDWARE -> fail("NO_HARDWARE", "No strong biometric authenticator")
            BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED -> fail("PERMISSION_MISSING", "Enroll a strong biometric locally")
            else -> fail("DEVICE_UNAVAILABLE", "Strong biometrics are unavailable")
        }
        val cancel = CancellationSignal()
        a.onStopAction { cancel.cancel() }
        BiometricPrompt.Builder(a).setTitle("Authenticate for linked Termux request").setAllowedAuthenticators(allowed)
            .setConfirmationRequired(true).setNegativeButton("Cancel", a.mainExecutor) { _, _ -> a.finish() }
            .build().authenticate(cancel, a.mainExecutor, object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                    a.finished(e, Result.success(obj("authenticated" to true, "method" to "strong-biometric")))
                }
                override fun onAuthenticationError(errorCode: Int, errString: CharSequence?) {
                    a.finished(e, Result.failure(OpError("CANCELLED", "Android did not complete biometric authentication")))
                }
            })
    }
    private fun record(a: EmbeddedConsentActivity, e: EmbeddedConsent.Request) {
        val file = File.createTempFile("embedded-mic-", ".m4a", a.cacheDir)
        val recorder = try { if (Build.VERSION.SDK_INT >= 31) MediaRecorder(a) else MediaRecorder() }
                       catch (ex: Exception) { file.delete(); throw ex }
        val timer = Handler(Looper.getMainLooper())
        var open = true
        fun close(output: Boolean) {
            if (!open) return
            open = false
            timer.removeCallbacksAndMessages(null)
            val stopped = runCatching { recorder.stop() }.isSuccess
            recorder.release()
            val result = runCatching {
                if (!stopped) fail("DEVICE_UNAVAILABLE", "Microphone recording could not be finalized")
                EmbeddedFiles.encoded(EmbeddedFiles.bytes(file.inputStream(), EmbeddedFiles.LIMIT), "audio/mp4")
            }
            file.delete()
            if (output) a.finished(e, result)
        }
        a.onStopAction { close(false) }
        try {
            recorder.setAudioSource(MediaRecorder.AudioSource.MIC)
            recorder.setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
            recorder.setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
            recorder.setAudioEncodingBitRate(64_000)
            recorder.setAudioChannels(1)
            recorder.setOutputFile(file.absolutePath)
            recorder.prepare(); recorder.start()
        } catch (ex: Exception) { close(false); throw ex }
        a.text("Microphone recording active")
        a.button("Stop and return audio") { close(true) }
        timer.postDelayed({ close(true) }, seconds(e.args, 12, 6) * 1000L)
    }
    private fun speech(a: EmbeddedConsentActivity, e: EmbeddedConsent.Request) {
        if (Build.VERSION.SDK_INT < 31 || !SpeechRecognizer.isOnDeviceRecognitionAvailable(a))
            fail("PLATFORM_RESTRICTED", "Android on-device speech recognition is unavailable; no online fallback")
        val r = SpeechRecognizer.createOnDeviceSpeechRecognizer(a)
        val timer = Handler(Looper.getMainLooper())
        var open = true
        fun close() { if (open) { open = false; timer.removeCallbacksAndMessages(null); r.cancel(); r.destroy() } }
        a.onStopAction { close() }
        a.text("On-device microphone recognition active")
        a.button("Stop listening") { if (open) r.stopListening() }
        r.setRecognitionListener(object : RecognitionListener {
            override fun onReadyForSpeech(params: Bundle?) {}
            override fun onBeginningOfSpeech() {}
            override fun onRmsChanged(rmsdB: Float) {}
            override fun onBufferReceived(buffer: ByteArray?) {}
            override fun onEndOfSpeech() {}
            override fun onPartialResults(partialResults: Bundle?) {}
            override fun onEvent(eventType: Int, params: Bundle?) {}
            override fun onResults(results: Bundle?) {
                if (!open) return
                val choices = results?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION).orEmpty().take(5).map { it.take(2000) }
                close(); a.finished(e, Result.success(obj("matches" to JSONArray(choices))))
            }
            override fun onError(error: Int) {
                if (!open) return
                close(); a.finished(e, Result.failure(OpError("DEVICE_UNAVAILABLE", "Android on-device recognition failed ($error)")))
            }
        })
        r.startListening(Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH).putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
            .putExtra(RecognizerIntent.EXTRA_PREFER_OFFLINE, true))
        timer.postDelayed({ close(); a.finished(e, Result.failure(OpError("DEVICE_UNAVAILABLE", "No on-device speech result within 15 seconds"))) }, 15_000)
    }
    private fun media(a: EmbeddedConsentActivity, e: EmbeddedConsent.Request) {
        a.openSystem(Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE).setType("audio/*").putExtra(Intent.EXTRA_LOCAL_ONLY, true)) { code, data ->
            if (code != Activity.RESULT_OK) fail("CANCELLED", "No audio selected")
            val uri = data?.data ?: fail("NOT_FOUND", "No selected audio document")
            if (uri.scheme != "content" || !android.provider.DocumentsContract.isDocumentUri(a, uri) || uri.authority == a.packageName)
                fail("INVALID_ARGUMENT", "Use a selected Android document")
            EmbeddedConsent.checkLive(e)
            val player = MediaPlayer()
            val timer = Handler(Looper.getMainLooper())
            var open = true
            fun close() { if (open) { open = false; timer.removeCallbacksAndMessages(null); player.release() } }
            fun finish() { close(); a.finished(e, Result.success(obj("playbackEnded" to true))) }
            a.onStopAction { close() }
            a.button("Stop playback") { if (player.isPlaying) finish() else a.finish() }
            player.setOnPreparedListener {
                val result = runCatching { EmbeddedConsent.checkLive(e); it.start() }
                if (result.isFailure) { close(); a.finished(e, Result.failure(result.exceptionOrNull()!!)) }
            }
            player.setOnCompletionListener { finish() }
            player.setOnErrorListener { _, _, _ -> close(); a.finished(e, Result.failure(OpError("DEVICE_UNAVAILABLE", "Android could not play this file"))); true }
            if (a.contentResolver.getType(uri) !in setOf("audio/mpeg", "audio/mp4", "audio/ogg", "audio/wav", "audio/x-wav", "audio/flac", "audio/aac"))
                fail("INVALID_ARGUMENT", "Choose a supported single audio document, not a playlist or stream")
            a.contentResolver.openAssetFileDescriptor(uri, "r")?.use { fd ->
                if (fd.declaredLength >= 0) player.setDataSource(fd.fileDescriptor, fd.startOffset, fd.declaredLength)
                else if (fd.startOffset == 0L) player.setDataSource(fd.fileDescriptor)
                else fail("DEVICE_UNAVAILABLE", "Provider returned an unsupported audio descriptor")
            } ?: fail("DEVICE_UNAVAILABLE", "Provider could not open selected audio")
            player.prepareAsync()
            timer.postDelayed({ if (player.isPlaying) finish() else a.finish() }, seconds(e.args, 60, 30) * 1000L)
        }
    }
    private fun nfc(a: EmbeddedConsentActivity, e: EmbeddedConsent.Request) {
        val adapter = NfcAdapter.getDefaultAdapter(a) ?: fail("NO_HARDWARE", "No NFC adapter")
        if (!adapter.isEnabled) fail("DEVICE_UNAVAILABLE", "NFC is off; enable it locally")
        val timer = Handler(Looper.getMainLooper())
        var scanning = true
        fun close() { if (scanning) { scanning = false; timer.removeCallbacksAndMessages(null); adapter.disableReaderMode(a) } }
        a.onStopAction { close() }
        a.text("Present one NFC tag")
        adapter.enableReaderMode(a, { tag ->
            MainThread.post {
                if (scanning) {
                    close()
                    val message = Ndef.get(tag)?.cachedNdefMessage?.toByteArray()
                    val result: Result<Any> = if (message != null && message.size > 32_768) Result.failure(OpError("DATA_LIMIT", "NFC data exceeds 32 KiB"))
                        else Result.success(obj("technologies" to JSONArray(tag.techList.toList()), "ndef" to message?.let { EmbeddedFiles.encoded(it, "application/octet-stream") }))
                    a.finished(e, result)
                }
            }
        }, NfcAdapter.FLAG_READER_NFC_A or NfcAdapter.FLAG_READER_NFC_B or NfcAdapter.FLAG_READER_NFC_F or NfcAdapter.FLAG_READER_NFC_V, null)
        timer.postDelayed({ close(); a.finished(e, Result.failure(OpError("NOT_FOUND", "No tag presented in 30 seconds"))) }, 30_000)
    }
}
