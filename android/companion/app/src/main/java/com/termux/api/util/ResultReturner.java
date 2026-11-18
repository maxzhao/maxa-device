// SPDX-License-Identifier: GPL-3.0-only
// Maxa (2026-11-18): replaces Termux:API's Unix-socket/broadcast transport with an in-process, bounded adapter.
// ResultWriter/ResultJsonWriter interfaces derived from termux-api fc26ce17e3badf85d4df191af364fcf7798047f1.
package com.termux.api.util;

import android.content.Intent;
import android.util.JsonWriter;
import java.io.IOException;
import java.io.PrintWriter;
import java.io.StringWriter;

/** Used only by the explicitly imported API implementations. No sockets, components or caller-provided Intent extras. */
public final class ResultReturner {
    public interface ResultWriter { void writeResult(PrintWriter out) throws Exception; }
    public abstract static class ResultJsonWriter implements ResultWriter {
        public final void writeResult(PrintWriter out) throws Exception {
            JsonWriter writer = new JsonWriter(out);
            writeJson(writer);
            writer.flush();
        }
        public abstract void writeJson(JsonWriter out) throws Exception;
    }
    private static final class BoundedWriter extends StringWriter {
        @Override public void write(String value, int offset, int length) {
            checkLimit(length); super.write(value, offset, length);
        }
        @Override public void write(char[] value, int offset, int length) {
            checkLimit(length); super.write(value, offset, length);
        }
        @Override public void write(int value) { checkLimit(1); super.write(value); }
        private void checkLimit(int added) {
            if (getBuffer().length() + added > 1024 * 1024) throw new IllegalStateException("API output exceeds 1 MiB character limit");
        }
    }
    private static final class Capture {
        final BoundedWriter output = new BoundedWriter();
        boolean complete;
    }
    private static final ThreadLocal<Capture> CURRENT = new ThreadLocal<>();
    public static String invoke(Runnable guard, Runnable upstream) {
        if (CURRENT.get() != null) throw new IllegalStateException("Nested API invocation is forbidden");
        Capture capture = new Capture();
        CURRENT.set(capture);
        try {
            guard.run(); upstream.run();
            if (!capture.complete) throw new IllegalStateException("Upstream did not return a result");
            return capture.output.toString();
        } finally { CURRENT.remove(); }
    }
    public static void returnData(Object ignoredCaller, Intent ignoredIntent, ResultWriter result) {
        Capture capture = CURRENT.get();
        if (capture == null || capture.complete) throw new IllegalStateException("API result outside an active authenticated invocation");
        PrintWriter writer = new PrintWriter(capture.output);
        try {
            if (result != null) result.writeResult(writer);
            writer.flush();
            if (writer.checkError()) throw new IOException("API output failed");
            capture.complete = true;
        } catch (RuntimeException e) { throw e; }
        catch (Exception e) { throw new IllegalStateException("Upstream API result failed", e); }
    }
    public static void noteDone(Object ignoredCaller, Intent intent) { returnData(ignoredCaller, intent, null); }
    private ResultReturner() {}
}
