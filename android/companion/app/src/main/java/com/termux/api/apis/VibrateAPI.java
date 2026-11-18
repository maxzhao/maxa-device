// SPDX-License-Identifier: GPL-3.0-only
// Maxa (2026-11-18): adapted from Termux:API fc26ce1 for the independent APK; see notices.
package com.termux.api.apis;

import android.content.Context;
import android.content.Intent;
import android.media.AudioAttributes;
import android.media.AudioManager;
import android.os.Build;
import android.os.VibrationEffect;
import android.os.Vibrator;

import com.termux.api.util.ResultReturner;
import com.maxa.device.companion.api.ApiLog;

public class VibrateAPI {

    private static final String LOG_TAG = "VibrateAPI";

    public static void onReceive(Object apiReceiver, Context context, Intent intent) {
        ApiLog.logDebug(LOG_TAG, "onReceive");

        Vibrator vibrator = context.getSystemService(Vibrator.class);
        AudioManager audio = context.getSystemService(AudioManager.class);
        if (audio.getRingerMode() == AudioManager.RINGER_MODE_SILENT) {
            ResultReturner.returnData(apiReceiver, intent, new ResultReturner.ResultJsonWriter() {
                public void writeJson(android.util.JsonWriter out) throws java.io.IOException {
                    out.beginObject().name("suppressed").value(true).endObject();
                }
            });
            return;
        }
        int milliseconds = intent.getIntExtra("duration_ms", 1000);
        vibrator.vibrate(VibrationEffect.createOneShot(milliseconds, VibrationEffect.DEFAULT_AMPLITUDE),
            new AudioAttributes.Builder().setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                .setUsage(AudioAttributes.USAGE_NOTIFICATION).build());
        ResultReturner.noteDone(apiReceiver, intent);
    }
}
