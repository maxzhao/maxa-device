// SPDX-License-Identifier: GPL-3.0-only
// Derived from Termux:API contributors, fc26ce17e3badf85d4df191af364fcf7798047f1
// Maxa modifications (2026-11-18): in-process bounded result transport; no broadcast receiver; local logging adapter.
// Original source/attribution and exact upstream hashes: licenses/termux-api/ORIGIN.json and README.upstream.md.
package com.termux.api.util;

import android.util.JsonWriter;

import com.maxa.device.companion.api.ApiLog;

import java.io.IOException;

public class JsonUtils {

    public static final String LOG_TAG = "JsonUtils";



    public static void putBooleanValueIfSet(JsonWriter out, String key, Boolean value) {
        if (out == null || key == null || key.isEmpty() || value == null) return;

        try {
            out.name(key).value(value);
        } catch (IOException e) {
            ApiLog.logStackTraceWithMessage(LOG_TAG, "Failed to put \"" + key + "\" with boolean value \"" + value + "\"", e);
        }
    }

    public static void putIntegerIfSet(JsonWriter out, String key, Integer value) {
        if (out == null || key == null || key.isEmpty() || value == null) return;

        try {
            out.name(key).value(value);
        } catch (IOException e) {
            ApiLog.logStackTraceWithMessage(LOG_TAG, "Failed to put \"" + key + "\" with integer value \"" + value + "\"", e);
        }
    }

    public static void putLongIfSet(JsonWriter out, String key, Long value) {
        if (out == null || key == null || key.isEmpty() || value == null) return;

        try {
            out.name(key).value(value);
        } catch (IOException e) {
            ApiLog.logStackTraceWithMessage(LOG_TAG, "Failed to put \"" + key + "\" with long value \"" + value + "\"", e);
        }
    }

    public static void putDoubleIfSet(JsonWriter out, String key, Double value) {
        if (out == null || key == null || key.isEmpty() || value == null) return;

        try {
            out.name(key).value(value);
        } catch (IOException e) {
            ApiLog.logStackTraceWithMessage(LOG_TAG, "Failed to put \"" + key + "\" with double value \"" + value + "\"", e);
        }
    }

    public static void putStringIfSet(JsonWriter out, String key, String value) {
        if (out == null || key == null || key.isEmpty() || value == null) return;

        try {
            out.name(key).value(value);
        } catch (IOException e) {
            ApiLog.logStackTraceWithMessage(LOG_TAG, "Failed to put \"" + key + "\" with string value \"" + value + "\"", e);
        }
    }

}
