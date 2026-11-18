// SPDX-License-Identifier: GPL-3.0-only
// Maxa (2026-11-18): adapted from Termux:API fc26ce1 for the independent APK; see notices.
package com.termux.api.apis;

import android.content.ContentResolver;
import android.content.Context;
import android.content.Intent;
import android.provider.Settings;

import com.termux.api.util.ResultReturner;
import com.maxa.device.companion.api.ApiLog;

public class BrightnessAPI {

    private static final String LOG_TAG = "BrightnessAPI";

    public static void onReceive(final Object receiver, final Context context, final Intent intent) {
        ApiLog.logDebug(LOG_TAG, "onReceive");

        final ContentResolver contentResolver = context.getContentResolver();
        if (intent.getBooleanExtra("auto", false)) {
            if (!Settings.System.putInt(contentResolver, Settings.System.SCREEN_BRIGHTNESS_MODE,
                    Settings.System.SCREEN_BRIGHTNESS_MODE_AUTOMATIC)) throw new SecurityException();
        } else {
            if (!Settings.System.putInt(contentResolver, Settings.System.SCREEN_BRIGHTNESS,
                    intent.getIntExtra("brightness", 0))) throw new SecurityException();
            if (!Settings.System.putInt(contentResolver, Settings.System.SCREEN_BRIGHTNESS_MODE,
                    Settings.System.SCREEN_BRIGHTNESS_MODE_MANUAL)) throw new IllegalStateException("Partial brightness change");
        }
        ResultReturner.noteDone(receiver, intent);
    }
}
