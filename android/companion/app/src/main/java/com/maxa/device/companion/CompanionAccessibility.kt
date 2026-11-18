package com.maxa.device.companion

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.graphics.*
import android.hardware.display.DisplayManager
import android.media.AudioManager
import android.annotation.TargetApi
import android.os.Build
import android.os.Bundle
import android.os.SystemClock
import android.view.inputmethod.EditorInfo
import android.view.Display
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import android.util.Xml
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.io.StringWriter
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit

class CompanionAccessibility : AccessibilityService() {
    companion object {
        @Volatile var instance: CompanionAccessibility? = null; private set
        private const val KEEP_AWAKE_MS = 10 * 60 * 1000L
        private val protectedPackages = setOf("com.android.packageinstaller", "com.google.android.packageinstaller", "com.miui.packageinstaller", "com.android.permissioncontroller", "com.google.android.permissioncontroller", "com.lbe.security.miui", "com.android.settings", "com.android.permissionui")
    }
    @Volatile private var lastPackage = ""
    @Volatile private var lastActivity = ""
    private var awakeView: android.view.View? = null
    private val awakeHandler = android.os.Handler(android.os.Looper.getMainLooper())
    private val releaseAwake = Runnable { releaseKeepAwake() }
    override fun onServiceConnected() { instance = this }
    override fun onDestroy() { releaseKeepAwake(); instance = null; super.onDestroy() }
    /**
     * Keeps the screen on while a controller drives the UI, so screen-off auto-lock cannot freeze Termux mid-control.
     * Uses an untouchable 1x1 accessibility overlay; never wakes the screen or dismisses the keyguard.
     * Released [KEEP_AWAKE_MS] after the last control call.
     */
    fun keepAwake() = MainThread.post {
        awakeHandler.removeCallbacks(releaseAwake)
        if (awakeView == null) {
            val view = android.view.View(this)
            val params = android.view.WindowManager.LayoutParams(1, 1, android.view.WindowManager.LayoutParams.TYPE_ACCESSIBILITY_OVERLAY,
                android.view.WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or android.view.WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE or android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON,
                PixelFormat.TRANSPARENT).apply { gravity = android.view.Gravity.START or android.view.Gravity.TOP }
            try { getSystemService(android.view.WindowManager::class.java).addView(view, params) } catch (_: RuntimeException) { return@post }
            awakeView = view
            CompanionRuntime.controlActive(true)
        }
        awakeHandler.postDelayed(releaseAwake, KEEP_AWAKE_MS)
    }
    private fun releaseKeepAwake() {
        awakeHandler.removeCallbacks(releaseAwake)
        val view = awakeView ?: return
        awakeView = null
        runCatching { getSystemService(android.view.WindowManager::class.java).removeViewImmediate(view) }
        CompanionRuntime.controlActive(false)
    }
    override fun onInterrupt() {}
    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        if (event?.eventType == AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED) {
            lastPackage = event.packageName?.toString() ?: ""; lastActivity = event.className?.toString() ?: ""
        }
    }
    fun screen(): JSONObject = MainThread.call {
        val display = getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY)
        val size = Point(); display.getRealSize(size)
        obj("width" to size.x, "height" to size.y, "rotation" to display.rotation)
    }
    fun foreground(): JSONObject = MainThread.call {
        val root = rootInActiveWindow ?: fail("OUTCOME_UNKNOWN", "Foreground window unavailable")
        try { val pkg = root.packageName?.toString() ?: ""; obj("package" to pkg, "activity" to if (lastPackage == pkg) lastActivity else "") } finally { root.recycle() }
    }
    /**
     * Must run on main immediately before every input effect. Fail closed when foreground cannot be inspected.
     * [navigation] (HOME/BACK/APP_SWITCH) may only dismiss a protected surface, never confirm it, so it is allowed there.
     */
    fun guard(token: CallToken, navigation: Boolean = false) {
        CompanionRuntime.gate("ui", token)
        if (EmbeddedConsent.protectedSurface) fail("HUMAN_CONFIRMATION_REQUIRED", "Local Android authorization in progress")
        if (navigation) return
        if (ConsentActivity.projectionPending.get()) fail("HUMAN_CONFIRMATION_REQUIRED", "MediaProjection consent requires a human")
        val roots = windows.filter { it.isActive || it.isFocused }.mapNotNull { it.root }.toMutableList()
        rootInActiveWindow?.let { roots.add(it) }
        // A visible companion surface may contain local capability switches or authorizations.
        // Do not inject input into it, including in split-screen with a different active window.
        val ownWindowVisible = windows.any { window ->
            val root = window.root ?: return@any false
            try { root.packageName?.toString() == packageName } finally { root.recycle() }
        }
        if (roots.isEmpty()) fail("OUTCOME_UNKNOWN", "Cannot inspect foreground; no input sent")
        try {
            if (ownWindowVisible || roots.any { it.packageName?.toString() in protectedPackages ||
                    it.packageName?.toString() in setOf(packageName, "com.android.systemui") })
                fail("HUMAN_CONFIRMATION_REQUIRED", "System confirmation or permission surface requires a human")
        } finally { roots.forEach { it.recycle() } }
    }
    fun hierarchy(): JSONObject = MainThread.call {
        val root = rootInActiveWindow ?: fail("OUTCOME_UNKNOWN", "Window content unavailable")
        val screen = screen(); val writer = StringWriter(); val xml = Xml.newSerializer(); xml.setOutput(writer)
        xml.startDocument("UTF-8", true); xml.startTag(null, "hierarchy"); xml.attribute(null, "rotation", screen.getInt("rotation").toString())
        var count = 0
        fun node(item: AccessibilityNodeInfo, index: Int, depth: Int) {
            if (++count > 10000 || depth > 100) fail("OUTCOME_UNKNOWN", "Hierarchy exceeds bounded traversal")
            xml.startTag(null, "node")
            fun attr(name: String, value: Any?) { xml.attribute(null, name, value?.toString() ?: "") }
            attr("index", index); attr("text", if (item.isPassword) "" else item.text); attr("resource-id", item.viewIdResourceName)
            attr("class", item.className); attr("package", item.packageName); attr("content-desc", if (item.isPassword) "" else item.contentDescription)
            attr("checkable", item.isCheckable); attr("checked", item.isChecked); attr("clickable", item.isClickable)
            attr("enabled", item.isEnabled); attr("focusable", item.isFocusable); attr("focused", item.isFocused)
            attr("scrollable", item.isScrollable); attr("long-clickable", item.isLongClickable); attr("password", item.isPassword); attr("selected", item.isSelected)
            val rect = Rect(); item.getBoundsInScreen(rect); attr("bounds", "[${rect.left},${rect.top}][${rect.right},${rect.bottom}]")
            for (i in 0 until item.childCount) item.getChild(i)?.let { child -> try { node(child, i, depth + 1) } finally { child.recycle() } }
            xml.endTag(null, "node")
        }
        try { node(root, 0, 0) } finally { root.recycle() }
        xml.endTag(null, "hierarchy"); xml.endDocument()
        screen.put("xml", writer.toString())
    }
    private fun focused(): AccessibilityNodeInfo {
        val root = rootInActiveWindow ?: fail("INVALID_ARGUMENT", "No observable editable focus; no input sent")
        val focus = try { root.findFocus(AccessibilityNodeInfo.FOCUS_INPUT) } finally { root.recycle() }
        if (focus == null || !focus.isEditable || !focus.isFocused || focus.isPassword || !focus.isEnabled) {
            focus?.recycle(); fail("INVALID_ARGUMENT", "Editable non-password focus required; no input sent")
        }
        return focus
    }
    private fun imeAvailable(node: AccessibilityNodeInfo) = node.actionList.any { it.id == AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.id }
    /**
     * Android 13+: commit through the accessibility input method so text lands at the editor's real
     * cursor/selection exactly like an IME. Earlier releases use the ACTION_SET_TEXT selection path.
     */
    fun type(text: String, submit: Boolean, token: CallToken): JSONObject {
        if (Build.VERSION.SDK_INT >= 33) {
            val committed = MainThread.call { commitThroughInputMethod(text, submit, token) }
            if (committed != null) {
                val deadline = SystemClock.elapsedRealtime() + 1500
                while (true) {
                    token.check()
                    if (MainThread.call { readBackContains(text) }) break
                    if (SystemClock.elapsedRealtime() > deadline) fail("OUTCOME_UNKNOWN", "Text read-back did not confirm input; do not replay")
                    Thread.sleep(100)
                }
                if (submit) MainThread.call { submitThroughInputMethod(committed, token) }
                return obj()
            }
        }
        return typeBySetText(text, submit, token)
    }
    @TargetApi(33)
    private fun submitThroughInputMethod(action: Int, token: CallToken) {
        guard(token)
        val ime = inputMethod; val connection = ime?.currentInputConnection
        if (connection == null || !ime.currentInputStarted) fail("OUTCOME_UNKNOWN", "Input session ended before submit; text was inserted")
        connection.performEditorAction(action)
    }
    /** Returns the IME action to use for submit, or null when no accessibility input session is active. */
    @TargetApi(33)
    private fun commitThroughInputMethod(text: String, submit: Boolean, token: CallToken): Int? {
        guard(token)
        val ime = inputMethod ?: return null
        val connection = ime.currentInputConnection ?: return null
        val editor = ime.currentInputEditorInfo ?: return null
        if (!ime.currentInputStarted) return null
        val node = focused()
        try {
            val action = editor.imeOptions and EditorInfo.IME_MASK_ACTION
            val usable = action != EditorInfo.IME_ACTION_NONE && action != EditorInfo.IME_ACTION_UNSPECIFIED
            if (submit && !usable && !imeAvailable(node)) fail("UNSUPPORTED", "IME enter action unavailable; no input sent")
            guard(token)
            connection.commitText(text, 1, null)
            return if (usable) action else EditorInfo.IME_ACTION_DONE
        } finally { node.recycle() }
    }
    private fun readBackContains(text: String): Boolean {
        val root = rootInActiveWindow ?: return false
        val focus = try { root.findFocus(AccessibilityNodeInfo.FOCUS_INPUT) } finally { root.recycle() } ?: return false
        return try { focus.refresh(); focus.text?.toString()?.contains(text) == true } finally { focus.recycle() }
    }
    private fun typeBySetText(text: String, submit: Boolean, token: CallToken) = MainThread.call {
        guard(token); val node = focused()
        try {
            if (submit && !imeAvailable(node)) fail("UNSUPPORTED", "IME enter action unavailable; no input sent")
            val before = node.text?.toString() ?: ""
            val start = node.textSelectionStart; val end = node.textSelectionEnd
            if (start !in 0..before.length || end !in start..before.length) fail("INVALID_ARGUMENT", "Selection unobservable; no input sent")
            val after = before.substring(0, start) + text + before.substring(end)
            guard(token)
            val args = Bundle().apply { putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, after) }
            if (!node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) fail("OUTCOME_UNKNOWN", "Text action not acknowledged")
            if (!node.refresh() || node.text?.toString() != after) fail("OUTCOME_UNKNOWN", "Text read-back did not match; do not replay")
            node.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, Bundle().apply {
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, start + text.length)
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, start + text.length)
            })
            if (submit) {
                guard(token)
                if (!node.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.id)) fail("OUTCOME_UNKNOWN", "IME enter was not acknowledged")
            }
            obj()
        } finally { node.recycle() }
    }
    fun button(button: String, token: CallToken): JSONObject = MainThread.call {
        if (button !in setOf("HOME", "BACK", "APP_SWITCH", "ENTER", "BACKSPACE", "VOLUME_UP", "VOLUME_DOWN")) fail("UNSUPPORTED", "Unsupported Android button: $button")
        guard(token, navigation = button in setOf("HOME", "BACK", "APP_SWITCH"))
        when (button) {
            "HOME", "BACK", "APP_SWITCH" -> {
                val action = when (button) { "HOME" -> GLOBAL_ACTION_HOME; "BACK" -> GLOBAL_ACTION_BACK; else -> GLOBAL_ACTION_RECENTS }
                if (!performGlobalAction(action)) fail("OUTCOME_UNKNOWN", "Global action was not acknowledged")
            }
            "ENTER" -> {
                val node = focused(); try {
                    if (!imeAvailable(node)) fail("UNSUPPORTED", "IME enter action unavailable")
                    if (!node.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.id)) fail("OUTCOME_UNKNOWN", "IME action not acknowledged")
                } finally { node.recycle() }
            }
            "BACKSPACE" -> {
                val node = focused(); try {
                    val before = node.text?.toString() ?: ""; val start = node.textSelectionStart; val end = node.textSelectionEnd
                    if (start !in 0..before.length || end !in start..before.length) fail("INVALID_ARGUMENT", "Selection unavailable")
                    val left = if (start == end && start > 0) before.offsetByCodePoints(start, -1) else start
                    val after = before.substring(0, left) + before.substring(end)
                    if (!node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, Bundle().apply { putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, after) }) || !node.refresh() || node.text?.toString() != after) fail("OUTCOME_UNKNOWN", "Deletion unverified")
                    node.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, Bundle().apply {
                        putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, left)
                        putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, left)
                    })
                } finally { node.recycle() }
            }
            else -> getSystemService(AudioManager::class.java).adjustVolume(if (button == "VOLUME_UP") AudioManager.ADJUST_RAISE else AudioManager.ADJUST_LOWER, 0)
        }
        obj()
    }
    fun gesture(x1: Float, y1: Float, x2: Float, y2: Float, duration: Long, token: CallToken): JSONObject {
        val done = CompletableFuture<Boolean>()
        MainThread.call {
            guard(token); val s = screen()
            if (listOf(x1, x2).any { !it.isFinite() || it < 0 || it >= s.getInt("width") } || listOf(y1, y2).any { !it.isFinite() || it < 0 || it >= s.getInt("height") } || duration !in 1..10000) fail("INVALID_ARGUMENT", "Gesture coordinates/duration invalid")
            val path = Path().apply { moveTo(x1, y1); if (x1 != x2 || y1 != y2) lineTo(x2, y2) }
            val gesture = GestureDescription.Builder().addStroke(GestureDescription.StrokeDescription(path, 0, duration)).build()
            if (!dispatchGesture(gesture, object : GestureResultCallback() {
                    override fun onCompleted(gestureDescription: GestureDescription?) { done.complete(true) }
                    override fun onCancelled(gestureDescription: GestureDescription?) { done.complete(false) }
                }, null)) fail("OUTCOME_UNKNOWN", "Gesture dispatch not acknowledged")
        }
        if (!done.get(duration + 5000, TimeUnit.MILLISECONDS)) fail("CANCELLED", "Gesture cancelled by Android")
        token.check(); return obj()
    }
    fun screenshot(token: CallToken): JSONObject {
        val result = CompletableFuture<JSONObject>()
        MainThread.call {
            CompanionRuntime.gate("ui", token)
            takeScreenshot(Display.DEFAULT_DISPLAY, mainExecutor, object : TakeScreenshotCallback {
                override fun onSuccess(screenshot: ScreenshotResult) {
                    try {
                        token.check()
                        val bitmap = Bitmap.wrapHardwareBuffer(screenshot.hardwareBuffer, screenshot.colorSpace) ?: fail("OUTCOME_UNKNOWN", "Screenshot unavailable")
                        try {
                            val png = ByteArrayOutputStream(); if (!bitmap.compress(Bitmap.CompressFormat.PNG, 100, png)) fail("OUTCOME_UNKNOWN", "Screenshot encoding failed")
                            val blob = CompanionRuntime.blobs.fromBytes(token.owner, "ui", png.toByteArray())
                            result.complete(obj("blobId" to blob.id, "size" to blob.file.length(), "width" to bitmap.width, "height" to bitmap.height, "format" to "png"))
                        } finally { bitmap.recycle() }
                    } catch (e: Exception) { result.completeExceptionally(e) } finally { screenshot.hardwareBuffer.close() }
                }
                override fun onFailure(errorCode: Int) { result.completeExceptionally(OpError("PERMISSION_MISSING", "Screenshot denied or secure window (Android error $errorCode)")) }
            })
        }
        return try { result.get(15, TimeUnit.SECONDS) } catch (e: java.util.concurrent.ExecutionException) { throw (e.cause ?: e) }
    }
}
