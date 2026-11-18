package com.maxa.device.companion

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.content.ClipData
import android.content.ClipboardManager
import android.content.pm.PackageManager
import android.content.res.ColorStateList
import android.content.res.Configuration
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.RippleDrawable
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Switch
import android.widget.TextView
import android.widget.Toast
import java.net.Inet4Address
import java.net.NetworkInterface

/** Setup wizard and status dashboard. All behavior goes through [CompanionController]. */
class MainActivity : Activity() {
    private enum class Tone(val fg: Int, val bg: Int) {
        OK(R.color.ok, R.color.ok_bg), WARN(R.color.warn, R.color.warn_bg),
        OFF(R.color.text_secondary, R.color.off_bg), INFO(R.color.primary, R.color.primary_bg)
    }
    private data class Capability(val name: String, val title: Int, val desc: Int, val grant: String?)

    private val capabilityRows = listOf(
        Capability("ui", R.string.cap_ui, R.string.cap_ui_desc, "ui"),
        Capability("recording", R.string.cap_recording, R.string.cap_recording_desc, null),
        Capability("install", R.string.cap_install, R.string.cap_install_desc, "install"),
        Capability("orientation", R.string.cap_orientation, R.string.cap_orientation_desc, "orientation"),
        Capability("clipboard", R.string.cap_clipboard, R.string.cap_clipboard_desc, null),
        Capability("usage", R.string.cap_usage, R.string.cap_usage_desc, "usage"),
    )
    private lateinit var controller: CompanionController
    private val handler = Handler(Looper.getMainLooper())
    private val updaters = mutableListOf<(CompanionState) -> Unit>()
    private val poll = object : Runnable {
        override fun run() { refresh(); handler.postDelayed(this, 2000) }
    }
    private val medium: Typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
    private val match = ViewGroup.LayoutParams.MATCH_PARENT
    private val wrap = ViewGroup.LayoutParams.WRAP_CONTENT

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        controller = CompanionController(this)
        runCatching { controller.start() }
        window.decorView.setBackgroundColor(color(R.color.background))
        val night = resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK == Configuration.UI_MODE_NIGHT_YES
        val light = WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS or WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS
        window.insetsController?.setSystemBarsAppearance(if (night) 0 else light, light)
        setContentView(content())
        refresh()
        if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            // The foreground-service notification keeps the loopback listener visible and less likely to be frozen.
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
        }
    }
    override fun onResume() { super.onResume(); handler.removeCallbacks(poll); handler.post(poll) }
    override fun onPause() { handler.removeCallbacks(poll); super.onPause() }

    private fun refresh() {
        val state = runCatching { controller.state() }.getOrNull() ?: return
        updaters.forEach { it(state) }
    }

    // ---- Layout -------------------------------------------------------------------------

    private fun content(): View {
        val column = maxWidthColumn(dp(760)).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(16), dp(20), dp(32))
        }
        column.addView(header())
        column.addView(trustCard())
        column.addView(termuxStep())
        column.addView(agentStep())
        column.addView(capabilityStep())
        column.addView(pairStep())
        column.addView(backgroundStep())
        column.addView(footer())
        val frame = FrameLayout(this).apply {
            addView(column, FrameLayout.LayoutParams(match, wrap, Gravity.CENTER_HORIZONTAL))
        }
        return ScrollView(this).apply {
            isFillViewport = true
            clipToPadding = false
            addView(frame, ViewGroup.LayoutParams(match, wrap))
            setOnApplyWindowInsetsListener { view, insets ->
                val bars = insets.getInsets(WindowInsets.Type.systemBars() or WindowInsets.Type.displayCutout())
                view.setPadding(bars.left, bars.top, bars.right, bars.bottom)
                WindowInsets.CONSUMED
            }
        }
    }

    private fun header(): View {
        val root = vertical().apply { setPadding(dp(4), dp(12), dp(4), dp(8)) }
        root.addView(label("Maxa Device", 26f, R.color.text, bold = true))
        val subtitle = label("", 14f, R.color.text_secondary).apply { setPadding(0, dp(2), 0, dp(12)) }
        root.addView(subtitle)
        val chips = horizontal()
        val link = chip(); val service = chip().apply { (layoutParams as LinearLayout.LayoutParams).marginStart = dp(8) }
        chips.addView(link); chips.addView(service); root.addView(chips)
        updaters += { s ->
            subtitle.text = getString(R.string.subtitle_version, s.version)
            if (s.linked) setChip(link, getString(R.string.chip_linked), Tone.OK) else setChip(link, getString(R.string.chip_unlinked), Tone.OFF)
            if (s.port != null) setChip(service, getString(R.string.chip_service_on, s.port), Tone.INFO)
            else setChip(service, getString(R.string.chip_service_off), Tone.WARN)
        }
        return root
    }

    private fun trustCard(): View = card(fill = R.color.warn_bg, stroke = R.color.warn_outline).apply {
        addView(label(getString(R.string.trust_title), 15f, R.color.warn, bold = true))
        addView(label(getString(R.string.trust_body), 14f, R.color.text).apply { setPadding(0, dp(6), 0, 0) })
    }

    private fun termuxStep(): View {
        val card = card()
        val chip = stepHeader(card, "1", R.string.step_termux)
        val termux = statusLine()
        card.addView(termux)
        val getTermux = buttonRow(
            button(R.string.get_termux_fdroid) { controller.openTermuxDownload("fdroid") },
            button(R.string.get_termux_github) { controller.openTermuxDownload("github") })
        card.addView(getTermux)
        updaters += { s ->
            val t = s.termux
            setStatus(termux, t.installed, R.string.termux_installed, R.string.termux_missing)
            getTermux.visibility = if (t.installed) View.GONE else View.VISIBLE
            if (t.ready) setChip(chip, getString(R.string.chip_ready), Tone.OK) else setChip(chip, getString(R.string.chip_action), Tone.WARN)
        }
        return card
    }

    private fun agentStep(): View {
        val card = card()
        val chip = stepHeader(card, "2", R.string.step_agent)
        card.addView(body(R.string.agent_desc))
        val placeholder = label(getString(R.string.payload_missing), 14f, R.color.warn).apply { setPadding(0, dp(10), 0, 0) }
        card.addView(placeholder)
        val start = button(R.string.start_service) { controller.start(); handler.postDelayed({ refresh() }, 600) }
        card.addView(start.withTopMargin(14))
        val copy = button(R.string.copy_install, primary = true) {
            controller.copyInstallCommand(); toast(getString(R.string.copied_install))
        }
        card.addView(copy.withTopMargin(14))
        val secondary = buttonRow(
            button(R.string.regenerate) { confirm(R.string.regenerate_confirm) { controller.regenerateLink() } },
            button(R.string.unlink) { confirm(R.string.unlink_confirm) { controller.unlink() } })
        card.addView(secondary)
        updaters += { s ->
            placeholder.visibility = if (s.payloadReady) View.GONE else View.VISIBLE
            start.visibility = if (s.port == null) View.VISIBLE else View.GONE
            setEnabled(copy, s.payloadReady && s.termux.ready && s.port != null)
            setEnabled(secondary.getChildAt(0), s.termux.ready)
            setEnabled(secondary.getChildAt(1), s.linked)
            if (s.linked) setChip(chip, getString(R.string.chip_linked_short), Tone.OK) else setChip(chip, getString(R.string.chip_unlinked), Tone.OFF)
        }
        return card
    }

    private fun capabilityStep(): View {
        val card = card()
        val chip = stepHeader(card, "3", R.string.step_capabilities)
        card.addView(body(R.string.capabilities_desc))
        capabilityRows.forEachIndexed { index, cap ->
            if (index > 0) card.addView(divider())
            card.addView(capabilityRow(cap))
        }
        card.addView(divider())
        card.addView(body(R.string.embedded_api_hint))
        card.addView(button(R.string.embedded_api_title) { showEmbeddedApiChoices() })
        card.addView(buttonRow(button(R.string.embedded_permissions) { showEmbeddedPermissions() },
            button(R.string.embedded_review) { act { controller.openEmbeddedReview() } }))
        updaters += { s ->
            val ready = s.capabilities.values.count { it.ready }
            setChip(chip, getString(R.string.chip_count, ready, s.capabilities.size), if (ready > 0) Tone.INFO else Tone.OFF)
        }
        return card
    }

    private fun showEmbeddedApiChoices() {
        val ids = EmbeddedApi.capabilityIds.sorted()
        val states = controller.state().capabilities
        val labels = ids.map { id ->
            val name = id.removePrefix("api.")
            val key = resources.getIdentifier("embedded_" + name.replace('-', '_'), "string", packageName)
            if (key == 0) name else "${getString(key)} · $name"
        }.toTypedArray()
        android.app.AlertDialog.Builder(this).setTitle(R.string.embedded_api_title)
            .setMultiChoiceItems(labels, ids.map { states[it]?.enabled == true }.toBooleanArray()) { _, index, checked ->
                act {
                    val id = ids[index]
                    controller.setCapability(id, checked)

                }
            }.setPositiveButton(android.R.string.ok, null).show()
    }
    private fun showEmbeddedPermissions() {
        val states = controller.state().capabilities
        val missing = EmbeddedApi.capabilityIds.filter { id -> states[id]?.let { it.enabled && !it.granted } == true }.sorted()
        val builder = android.app.AlertDialog.Builder(this).setTitle(R.string.embedded_permissions)
        if (missing.isEmpty()) builder.setMessage(R.string.embedded_no_grants)
        else builder.setItems(missing.map { it.removePrefix("api.") }.toTypedArray()) { _, i ->
            act { controller.openEmbeddedPermission(missing[i]) }
        }
        builder.setNegativeButton(android.R.string.cancel, null).show()
    }
    private fun capabilityRow(cap: Capability): View {
        val root = vertical().apply { setPadding(0, dp(14), 0, dp(14)) }
        val top = horizontal().apply { gravity = Gravity.CENTER_VERTICAL }
        val texts = vertical().apply { layoutParams = LinearLayout.LayoutParams(0, wrap, 1f) }
        texts.addView(label(getString(cap.title), 16f, R.color.text, bold = true))
        texts.addView(label(getString(cap.desc), 13f, R.color.text_secondary).apply { setPadding(0, dp(2), dp(12), 0) })
        top.addView(texts)
        var updating = false
        val toggle = Switch(this).apply {
            val states = arrayOf(intArrayOf(android.R.attr.state_checked), intArrayOf())
            thumbTintList = ColorStateList(states, intArrayOf(color(R.color.primary), color(R.color.switch_thumb_off)))
            trackTintList = ColorStateList(states, intArrayOf(color(R.color.primary_track), color(R.color.switch_track_off)))
        }
        top.addView(toggle); root.addView(top)
        val status = horizontal().apply { gravity = Gravity.CENTER_VERTICAL; setPadding(0, dp(8), 0, 0) }
        val chip = chip(); status.addView(chip)
        val grant = link(R.string.grant) { cap.grant?.let(controller::openSettings) }
        status.addView(grant)
        val restricted = link(R.string.restricted_settings) { controller.openSettings("app") }
        status.addView(restricted)
        root.addView(status)
        val hint = label(getString(R.string.restricted_hint), 12f, R.color.text_secondary).apply { setPadding(0, dp(6), 0, 0) }
        root.addView(hint)
        toggle.setOnCheckedChangeListener { _, checked ->
            if (updating) return@setOnCheckedChangeListener
            act {
                controller.setCapability(cap.name, checked)
                val state = controller.state().capabilities[cap.name]
                if (checked && state != null && !state.granted && cap.grant != null) controller.openSettings(cap.grant)
            }
        }
        updaters += { s ->
            val state = s.capabilities[cap.name]
            if (state != null) {
                updating = true; toggle.isChecked = state.enabled; updating = false
                when {
                    state.ready -> setChip(chip, getString(R.string.chip_ready), Tone.OK)
                    state.enabled -> setChip(chip, getString(R.string.chip_needs_grant), Tone.WARN)
                    else -> setChip(chip, getString(R.string.chip_off), Tone.OFF)
                }
                val needsGrant = state.enabled && !state.granted && cap.grant != null
                val notConnected = needsGrant && cap.name == "ui" && s.accessibilityEnabledInSystem
                grant.visibility = if (needsGrant) View.VISIBLE else View.GONE
                restricted.visibility = if (needsGrant && cap.name == "ui" && !notConnected) View.VISIBLE else View.GONE
                hint.text = getString(if (notConnected) R.string.ui_not_connected_hint else R.string.restricted_hint)
                hint.setTextColor(color(if (notConnected) R.color.warn else R.color.text_secondary))
                hint.visibility = if (needsGrant && cap.name == "ui") View.VISIBLE else View.GONE
                if (notConnected) setChip(chip, getString(R.string.chip_not_connected), Tone.WARN)
            }
        }
        return root
    }

    private fun pairStep(): View {
        val card = card()
        val chip = stepHeader(card, "4", R.string.step_pair)
        setChip(chip, getString(R.string.chip_in_termux), Tone.INFO)
        card.addView(body(R.string.pair_desc))
        listOf("maxa-device module enable system", "maxa-device module enable mobile",
            "maxa-device pair list", "maxa-device pair approve <id>").forEach { card.addView(code(it)) }
        card.addView(label(getString(R.string.pair_address), 13f, R.color.text_secondary, bold = true).apply { setPadding(0, dp(16), 0, 0) })
        val address = lanAddress()
        if (address != null) card.addView(code("$address:19427"))
        else card.addView(label(getString(R.string.pair_address_unknown), 14f, R.color.text_secondary).apply { setPadding(0, dp(6), 0, 0) })
        return card
    }

    private fun backgroundStep(): View {
        val card = card()
        val chip = stepHeader(card, "5", R.string.step_background)
        card.addView(body(R.string.background_desc))
        val companionLine = statusLine()
        card.addView(companionLine)
        val exempt = button(R.string.allow_companion_background, primary = true) { controller.requestBatteryExemption() }
        card.addView(exempt.withTopMargin(12))
        card.addView(label(getString(R.string.termux_background_title), 13f, R.color.text_secondary, bold = true).apply { setPadding(0, dp(18), 0, 0) })
        card.addView(code("termux-wake-lock"))
        card.addView(button(R.string.battery_settings) { controller.openSettings("battery") }.withTopMargin(12))
        updaters += { s ->
            setStatus(companionLine, s.batteryExempt, R.string.companion_background_ok, R.string.companion_background_missing)
            exempt.visibility = if (s.batteryExempt) View.GONE else View.VISIBLE
            if (s.batteryExempt) setChip(chip, getString(R.string.chip_ready), Tone.OK) else setChip(chip, getString(R.string.chip_action), Tone.WARN)
        }
        return card
    }

    private fun footer(): View = vertical().apply {
        setPadding(0, dp(20), 0, 0)
        addView(link(R.string.stop_all, R.color.danger) {
            confirm(R.string.stop_confirm) { controller.stop(); handler.postDelayed({ refresh() }, 600) }
        }.apply { gravity = Gravity.CENTER; layoutParams = LinearLayout.LayoutParams(match, wrap) })
    }

    // ---- Components ---------------------------------------------------------------------

    private fun maxWidthColumn(maxWidth: Int): LinearLayout = object : LinearLayout(this) {
        override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
            val size = MeasureSpec.getSize(widthMeasureSpec)
            val spec = if (size > maxWidth) MeasureSpec.makeMeasureSpec(maxWidth, MeasureSpec.EXACTLY) else widthMeasureSpec
            super.onMeasure(spec, heightMeasureSpec)
        }
    }

    private fun card(fill: Int = R.color.surface, stroke: Int = R.color.outline) = vertical().apply {
        background = rounded(color(fill), dp(18), color(stroke))
        setPadding(dp(20), dp(18), dp(20), dp(18))
        layoutParams = LinearLayout.LayoutParams(match, wrap).apply { topMargin = dp(14) }
    }

    private fun stepHeader(card: LinearLayout, number: String, title: Int): TextView {
        val row = horizontal().apply { gravity = Gravity.CENTER_VERTICAL }
        row.addView(label(number, 13f, R.color.on_primary, bold = true).apply {
            gravity = Gravity.CENTER
            background = rounded(color(R.color.primary), dp(13))
            layoutParams = LinearLayout.LayoutParams(dp(26), dp(26))
        })
        row.addView(label(getString(title), 18f, R.color.text, bold = true).apply {
            layoutParams = LinearLayout.LayoutParams(0, wrap, 1f).apply { marginStart = dp(12); marginEnd = dp(8) }
        })
        val chip = chip()
        row.addView(chip)
        card.addView(row)
        return chip
    }

    private fun chip() = label("", 12f, R.color.text_secondary, bold = true).apply {
        setPadding(dp(10), dp(4), dp(10), dp(4))
        layoutParams = LinearLayout.LayoutParams(wrap, wrap)
    }
    private fun setChip(chip: TextView, text: String, tone: Tone) {
        chip.text = text; chip.setTextColor(color(tone.fg)); chip.background = rounded(color(tone.bg), dp(12))
    }

    private fun statusLine() = label("", 15f, R.color.text).apply { setPadding(0, dp(10), 0, 0) }
    private fun setStatus(view: TextView, ok: Boolean, okText: Int, badText: Int) {
        view.text = (if (ok) "✓  " else "!  ") + getString(if (ok) okText else badText)
        view.setTextColor(color(if (ok) R.color.text else R.color.warn))
    }

    private fun body(text: Int) = label(getString(text), 14f, R.color.text_secondary).apply { setPadding(0, dp(10), 0, 0) }

    private fun code(command: String) = horizontal().apply {
        gravity = Gravity.CENTER_VERTICAL
        background = rounded(color(R.color.code_bg), dp(10))
        setPadding(dp(14), dp(4), dp(4), dp(4))
        layoutParams = LinearLayout.LayoutParams(match, wrap).apply { topMargin = dp(8) }
        addView(label(command, 13f, R.color.text).apply {
            typeface = Typeface.MONOSPACE
            layoutParams = LinearLayout.LayoutParams(0, wrap, 1f)
        })
        addView(link(R.string.copy) { copyPlain(command); toast(getString(R.string.copied)) })
    }

    private fun button(text: Int, primary: Boolean = false, onClick: () -> Unit) = label(getString(text), 15f,
        if (primary) R.color.on_primary else R.color.primary, bold = true).apply {
        gravity = Gravity.CENTER
        minHeight = dp(48)
        setPadding(dp(16), dp(12), dp(16), dp(12))
        background = RippleDrawable(ColorStateList.valueOf(color(R.color.ripple)),
            rounded(color(if (primary) R.color.primary else R.color.button_secondary), dp(12)), null)
        isClickable = true; isFocusable = true
        layoutParams = LinearLayout.LayoutParams(match, wrap)
        setOnClickListener { act(onClick) }
    }

    private fun buttonRow(first: View, second: View) = horizontal().apply {
        layoutParams = LinearLayout.LayoutParams(match, wrap).apply { topMargin = dp(10) }
        addView(first, LinearLayout.LayoutParams(0, wrap, 1f).apply { marginEnd = dp(5) })
        addView(second, LinearLayout.LayoutParams(0, wrap, 1f).apply { marginStart = dp(5) })
    }

    private fun link(text: Int, tint: Int = R.color.primary, onClick: () -> Unit) = label(getString(text), 14f, tint, bold = true).apply {
        setPadding(dp(12), dp(10), dp(12), dp(10))
        background = RippleDrawable(ColorStateList.valueOf(color(R.color.ripple)), null, rounded(0xFFFFFFFF.toInt(), dp(10)))
        isClickable = true; isFocusable = true
        setOnClickListener { act(onClick) }
    }

    private fun divider() = View(this).apply {
        setBackgroundColor(color(R.color.outline))
        layoutParams = LinearLayout.LayoutParams(match, dp(1))
    }

    private fun label(value: CharSequence, size: Float, tint: Int, bold: Boolean = false) = TextView(this).apply {
        text = value
        setTextSize(TypedValue.COMPLEX_UNIT_SP, size)
        setTextColor(color(tint))
        setLineSpacing(0f, 1.15f)
        if (bold) typeface = medium
    }

    private fun vertical() = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
    private fun horizontal() = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
    private fun View.withTopMargin(value: Int) = apply { layoutParams = LinearLayout.LayoutParams(match, wrap).apply { topMargin = dp(value) } }
    private fun setEnabled(view: View, enabled: Boolean) { view.isEnabled = enabled; view.alpha = if (enabled) 1f else 0.4f }
    private fun rounded(fill: Int, radius: Int, stroke: Int? = null) = GradientDrawable().apply {
        setColor(fill); cornerRadius = radius.toFloat(); if (stroke != null) setStroke(dp(1), stroke)
    }

    // ---- Actions ------------------------------------------------------------------------

    private fun act(block: () -> Unit) {
        try { block() } catch (e: Exception) { toast(e.message ?: getString(R.string.action_failed)) }
        refresh()
    }
    private fun confirm(message: Int, block: () -> Unit) {
        AlertDialog.Builder(this).setMessage(message)
            .setPositiveButton(R.string.confirm) { _, _ -> act(block) }
            .setNegativeButton(R.string.cancel, null).show()
    }
    private fun copyPlain(text: String) {
        getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("Maxa Device", text))
    }
    private fun toast(message: String?) { Toast.makeText(this, message ?: getString(R.string.action_failed), Toast.LENGTH_LONG).show() }
    private fun color(id: Int) = getColor(id)
    private fun dp(value: Int) = (value * resources.displayMetrics.density + 0.5f).toInt()

    /** Wi-Fi IPv4 for manual pairing; prefers wlan interfaces over VPN tunnels. */
    private fun lanAddress(): String? = runCatching {
        NetworkInterface.getNetworkInterfaces().toList()
            .filter { it.isUp && !it.isLoopback }
            .sortedBy { if (it.name.startsWith("wlan")) 0 else 1 }
            .filter { it.name.startsWith("wlan") || it.name.startsWith("eth") }
            .flatMap { it.inetAddresses.toList() }
            .firstOrNull { it is Inet4Address && it.isSiteLocalAddress }?.hostAddress
    }.getOrNull()
}
