package com.maxa.device.companion

import android.app.Activity
import android.app.Service
import android.content.*
import android.graphics.Point
import android.hardware.display.*
import android.media.MediaRecorder
import android.media.projection.*
import android.os.*
import android.view.Display
import org.json.JSONObject
import java.io.File
import java.util.concurrent.*

class RecordingManager(private val context: Context) {
    init { context.filesDir.listFiles()?.filter { it.name.startsWith("record-") && it.name.endsWith(".mp4") }?.forEach { it.delete() } }
    private class Recording(val id: String, val token: CallToken, val limit: Int?, val file: File) {
        var projection: MediaProjection? = null
        var recorder: MediaRecorder? = null
        var display: VirtualDisplay? = null
        var startMs = 0L
        var result: JSONObject? = null
        val started = CompletableFuture<Unit>()
    }
    @Volatile private var active: Recording? = null
    fun isActive() = active != null
    fun start(args: JSONObject, token: CallToken): JSONObject {
        val limit = if (args.has("timeLimitSec")) args.getInt("timeLimitSec").also { if (it !in 1..3600) fail("INVALID_ARGUMENT", "timeLimitSec must be 1..3600") } else null
        val recording = MainThread.call {
            if (EmbeddedConsent.protectedSurface) fail("HUMAN_CONFIRMATION_REQUIRED", "Local Android review in progress")
            CompanionRuntime.gate("recording", token)
            if (active != null) fail("BUSY", "Only one recording may be active; stop the previous recording first")
            val id = Protocol.hex(Protocol.randomBytes(16))
            Recording(id, token, limit, File(context.filesDir, "record-$id.mp4")).also { active = it }
        }
        try {
            val (result, data) = Prompts.request("recording", null, token, 120)
            if (result != Activity.RESULT_OK || data == null) fail("PERMISSION_MISSING", "Human declined MediaProjection consent")
            token.check(); CompanionRuntime.gate("recording", token)
            MainThread.call {
                token.check()
                context.startForegroundService(Intent(context, RecordingService::class.java).putExtra("id", recording.id).putExtra("consent", data))
            }
            val deadline = SystemClock.elapsedRealtime() + 20_000
            while (true) {
                token.check()
                try { recording.started.get(200, TimeUnit.MILLISECONDS); break } catch (_: TimeoutException) {
                    if (SystemClock.elapsedRealtime() >= deadline) fail("OUTCOME_UNKNOWN", "Recording start not observed")
                } catch (e: ExecutionException) { throw (e.cause ?: e) }
            }
            return obj("recordingId" to recording.id)
        } catch (e: Exception) {
            MainThread.call { if (active === recording) discard() }; throw e
        }
    }
    /** Invoked only by the mediaProjection foreground service after startForeground. */
    fun setup(id: String, consent: Intent) {
        val r = active?.takeIf { it.id == id } ?: run { context.stopService(Intent(context, RecordingService::class.java)); return }
        try {
            CompanionRuntime.gate("recording", r.token)
            val projection = context.getSystemService(MediaProjectionManager::class.java).getMediaProjection(Activity.RESULT_OK, consent)
            r.projection = projection
            projection.registerCallback(object : MediaProjection.Callback() { override fun onStop() { if (active === r && r.recorder != null) finish(r) } }, Handler(Looper.getMainLooper()))
            val display = context.getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY)
            val size = Point(); display.getRealSize(size)
            val width = size.x / 2 * 2; val height = size.y / 2 * 2
            val recorder = MediaRecorder(); r.recorder = recorder
            recorder.setVideoSource(MediaRecorder.VideoSource.SURFACE)
            recorder.setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
            recorder.setVideoEncoder(MediaRecorder.VideoEncoder.H264)
            recorder.setVideoSize(width, height); recorder.setVideoFrameRate(30); recorder.setVideoEncodingBitRate(8_000_000)
            recorder.setOutputFile(r.file.absolutePath)
            recorder.setMaxFileSize(512L * 1024 * 1024)
            r.limit?.let { recorder.setMaxDuration(it * 1000) }
            recorder.setOnInfoListener { _, what, _ -> if (what == MediaRecorder.MEDIA_RECORDER_INFO_MAX_DURATION_REACHED || what == MediaRecorder.MEDIA_RECORDER_INFO_MAX_FILESIZE_REACHED) finish(r) }
            recorder.setOnErrorListener { _, _, _ -> r.started.completeExceptionally(OpError("OUTCOME_UNKNOWN", "Android recorder failed")); discard() }
            recorder.prepare()
            r.display = projection.createVirtualDisplay("Maxa recording", width, height, context.resources.displayMetrics.densityDpi, DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, recorder.surface, null, null)
            r.token.check(); recorder.start(); r.startMs = SystemClock.elapsedRealtime(); r.started.complete(Unit)
        } catch (e: Exception) { r.started.completeExceptionally(e); discard() }
    }
    private fun release(r: Recording) {
        val recorder = r.recorder; r.recorder = null
        try { recorder?.reset(); recorder?.release() } catch (_: Exception) {}
        r.display?.release(); r.display = null
        val projection = r.projection; r.projection = null
        try { projection?.stop() } catch (_: Exception) {}
        context.stopService(Intent(context, RecordingService::class.java))
    }
    private fun finish(r: Recording) {
        if (r.result != null || r.recorder == null) return
        val duration = SystemClock.elapsedRealtime() - r.startMs
        try {
            r.recorder!!.stop(); release(r)
            if (r.file.length() == 0L) fail("OUTCOME_UNKNOWN", "Empty recording")
            val blob = CompanionRuntime.blobs.fromFile(r.token.owner, "recording", r.file)
            r.result = obj("blobId" to blob.id, "size" to blob.file.length(), "durationMs" to duration, "format" to "mp4")
        } catch (_: Exception) { release(r); r.file.delete(); active = null }
    }
    fun stop(id: String, token: CallToken): JSONObject = MainThread.call {
        token.check(); val r = active ?: fail("NOT_FOUND", "No owned recording")
        if (r.id != id || r.token.owner != token.owner) fail("NOT_FOUND", "Recording is not owned by this session")
        if (!r.started.isDone) fail("BUSY", "Recording consent/start still pending")
        finish(r); val result = r.result ?: fail("OUTCOME_UNKNOWN", "Recording finalization failed")
        active = null; result
    }
    private fun discard() { active?.let { release(it); it.file.delete(); it.started.completeExceptionally(OpError("CANCELLED", "Recording cancelled")) }; active = null }
    fun invalidate() { MainThread.post { discard() } }
    fun cancelOwner(owner: String) { MainThread.post { if (active?.token?.owner == owner) discard() } }
    fun serviceDestroyed(id: String?) { MainThread.post { active?.takeIf { it.id == id && it.recorder != null }?.let { finish(it) } } }
}

class RecordingService : Service() {
    private var recordingId: String? = null
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForeground(11, foregroundNotification(this, "Screen recording (human-approved)"))
        val id = intent?.getStringExtra("id"); val data = intent?.getParcelableExtra<Intent>("consent")
        recordingId = id
        if (id != null && data != null) CompanionRuntime.operations.recording.setup(id, data) else stopSelf()
        return START_NOT_STICKY
    }
    override fun onDestroy() { CompanionRuntime.operations.recording.serviceDestroyed(recordingId); super.onDestroy() }
    override fun onBind(intent: Intent?) = null
}
