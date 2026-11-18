// SPDX-License-Identifier: GPL-3.0-only
// Maxa: restricted ports of Termux:API TextToSpeechAPI and JobSchedulerAPI paths.
package com.maxa.device.companion

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.job.JobInfo
import android.app.job.JobParameters
import android.app.job.JobScheduler
import android.app.job.JobService
import android.content.ComponentName
import android.content.Context
import android.content.pm.ApplicationInfo
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.speech.tts.TextToSpeech
import android.speech.tts.UtteranceProgressListener
import org.json.JSONObject

internal object EmbeddedLocalEffects {
    val names = setOf("tts-speak", "job-scheduler")
    private fun action(a: JSONObject) = a.text("action", 12)
    private fun delay(a: JSONObject) = a.int("delaySeconds", 5..120)
    fun describe(name: String, args: JSONObject): String = "Requested by linked Termux software:\n\n" + when (name) {
        "tts-speak" -> {
            args.only("text"); val value = args.text("text", 400)
            if (value.any { it.isISOControl() && it != '\n' || Character.getType(it) == Character.FORMAT.toInt() })
                fail("INVALID_ARGUMENT", "Text contains unsupported control characters")
            "Read this text aloud using a preinstalled engine with an offline voice.\n\n$value"
        }
        "job-scheduler" -> {
            when (action(args)) {
                "schedule" -> { args.only("action", "delaySeconds"); "Schedule one fixed local reminder no earlier than ${delay(args)} seconds. No custom text, commands or network." }
                "pending", "cancel" -> { args.only("action"); "${action(args)} the bounded local reminder." }
                else -> fail("INVALID_ARGUMENT", "Use schedule, pending or cancel")
            }
        }
        else -> error("Unmapped embedded local effect")
    }
    fun execute(a: EmbeddedConsentActivity, r: EmbeddedConsent.Request) {
        when (r.name) {
            "tts-speak" -> speak(a, r)
            "job-scheduler" -> a.finished(r, runCatching { EmbeddedReminder.apply(a, r) })
            else -> error("Unmapped embedded local effect")
        }
    }
    private fun speak(a: EmbeddedConsentActivity, r: EmbeddedConsent.Request) {
        val pm = a.packageManager
        val engines = pm.queryIntentServices(android.content.Intent(TextToSpeech.Engine.INTENT_ACTION_TTS_SERVICE), 0)
        val system = engines.filter { it.serviceInfo.applicationInfo.flags and ApplicationInfo.FLAG_SYSTEM != 0 }
        val selected = system.sortedBy { it.serviceInfo.packageName }.firstOrNull()?.serviceInfo?.packageName
            ?: fail("PLATFORM_RESTRICTED", "No preinstalled TTS engine is visible; no external engine fallback")
        // An explicit package does not by itself prevent Android from falling back to a different engine.
        // Require the same preinstalled engine to also be the system-configured default before initializing.
        if (android.provider.Settings.Secure.getString(a.contentResolver, "tts_default_synth") != selected)
            fail("PLATFORM_RESTRICTED", "Select a preinstalled Android TTS engine locally before speaking")
        val handler = Handler(Looper.getMainLooper())
        var engine: TextToSpeech? = null
        var stopped = false
        fun close() { if (!stopped) { stopped = true; handler.removeCallbacksAndMessages(null); engine?.stop(); engine?.shutdown() } }
        fun failVoice() { close(); a.finished(r, Result.failure(OpError("DEVICE_UNAVAILABLE", "Offline TTS voice not ready; no network voice fallback"))) }
        a.onStopAction { close() }
        a.button("Stop speech") { close(); a.finish() }
        if (system.size != 1 || engines.size != 1) fail("PLATFORM_RESTRICTED", "Offline speech is only enabled with one visible preinstalled TTS engine; Android may otherwise fall back")
        engine = TextToSpeech(a, { status -> MainThread.post {
            if (!stopped) {
                val tts = engine
                if (status != TextToSpeech.SUCCESS || tts == null) failVoice()
                else {
                    val result = runCatching {
                        EmbeddedConsent.checkLive(r)
                        val voice = tts.voices.orEmpty().filter { !it.isNetworkConnectionRequired &&
                            it.features?.contains(TextToSpeech.Engine.KEY_FEATURE_NOT_INSTALLED) != true && it.locale.language == java.util.Locale.getDefault().language }
                            .minByOrNull { it.name } ?: fail("PLATFORM_RESTRICTED", "No installed offline voice for device language")
                        if (tts.setVoice(voice) != TextToSpeech.SUCCESS || tts.voice?.name != voice.name) fail("DEVICE_UNAVAILABLE", "Offline voice not selected")
                        tts.setOnUtteranceProgressListener(object : UtteranceProgressListener() {
                            override fun onStart(utteranceId: String?) {}
                            override fun onDone(utteranceId: String?) { MainThread.post { if (!stopped) { close(); a.finished(r, Result.success(obj("spoken" to true))) } } }
                            override fun onError(utteranceId: String?) { MainThread.post { if (!stopped) failVoice() } }
                        })
                        if (tts.speak(r.args.text("text", 400), TextToSpeech.QUEUE_FLUSH, null, r.id) != TextToSpeech.SUCCESS)
                            fail("DEVICE_UNAVAILABLE", "Android did not accept speech")
                    }
                    if (result.isFailure) { close(); a.finished(r, Result.failure(result.exceptionOrNull()!!)) }
                }
            }
        } }, selected)
        handler.postDelayed({ if (!stopped) { close(); a.finished(r, Result.failure(OpError("OUTCOME_UNKNOWN", "Speech timed out and was stopped; do not replay"))) } }, 20_000)
    }
}

internal object EmbeddedReminder {
    private const val ID = 1_806_526_401
    private const val CHANNEL = "embedded-fixed-reminder"
    private var epoch: Long? = null
    private var validUntil = 0L
    @Synchronized fun cancel(context: Context) {
        epoch = null; validUntil = 0
        runCatching { context.getSystemService(JobScheduler::class.java)?.cancel(ID) }
    }
    @Synchronized fun apply(ctx: Context, r: EmbeddedConsent.Request): Any {
        EmbeddedConsent.checkLive(r)
        val scheduler = ctx.getSystemService(JobScheduler::class.java)
        return when (r.args.text("action", 12)) {
            "pending" -> obj("pending" to (epoch == r.epoch && scheduler.getPendingJob(ID) != null))
            "cancel" -> { cancel(ctx); obj("pending" to false) }
            else -> {
                if (!ctx.getSystemService(NotificationManager::class.java).areNotificationsEnabled()) fail("PERMISSION_MISSING", "Enable APK notifications first")
                if (scheduler.getPendingJob(ID) != null) fail("BUSY", "Only one reminder may be pending")
                val delay = r.args.int("delaySeconds", 5..120) * 1000L
                val job = JobInfo.Builder(ID, ComponentName(ctx, EmbeddedReminderJob::class.java)).setMinimumLatency(delay).build()
                epoch = r.epoch; validUntil = SystemClock.elapsedRealtime() + delay + 60_000
                if (scheduler.schedule(job) != JobScheduler.RESULT_SUCCESS) { epoch = null; fail("DEVICE_UNAVAILABLE", "Android rejected the job") }
                obj("scheduled" to true, "delayMs" to delay, "expiresAfterMs" to delay + 60_000)
            }
        }
    }
    @Synchronized fun dispatch(ctx: Context) {
        val scheduledEpoch = epoch ?: return
        epoch = null
        if (scheduledEpoch != CompanionRuntime.authEpoch) return
        if (SystemClock.elapsedRealtime() >= validUntil || !CompanionRuntime.linked || !CompanionRuntime.enabled("api.job-scheduler")) return
        val nm = ctx.getSystemService(NotificationManager::class.java)
        if (!nm.areNotificationsEnabled()) return
        nm.createNotificationChannel(NotificationChannel(CHANNEL, "Embedded API reminders", NotificationManager.IMPORTANCE_LOW))
        nm.notify(CHANNEL, ID, Notification.Builder(ctx, CHANNEL).setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentTitle("Maxa Device reminder").setContentText("Approved local reminder is due").setAutoCancel(true).build())
    }
}

class EmbeddedReminderJob : JobService() {
    override fun onStartJob(params: JobParameters): Boolean { EmbeddedReminder.dispatch(this); return false }
    override fun onStopJob(params: JobParameters) = false
}
