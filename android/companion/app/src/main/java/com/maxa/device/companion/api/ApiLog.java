// SPDX-License-Identifier: GPL-3.0-only
// Maxa local adapter for imported Termux:API diagnostics (2026-11-18).
package com.maxa.device.companion.api;

import android.util.Log;

/** Do not persist API result values, request arguments or exception contents in Android logs. */
public final class ApiLog {
    public static void logDebug(String tag, String ignoredMessage) { Log.d("MaxaApi", tag); }
    public static void logVerbose(String tag, String ignoredMessage) { Log.v("MaxaApi", tag); }
    public static void logError(String tag, String ignoredMessage) { throw new IllegalStateException("Upstream " + tag + " reported an error"); }
    public static void logStackTraceWithMessage(String tag, String ignoredMessage, Throwable error) {
        throw new IllegalStateException("Upstream " + tag + " result failed", error);
    }
    private ApiLog() {}
}
