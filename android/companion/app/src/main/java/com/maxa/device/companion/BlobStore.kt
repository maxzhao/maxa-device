package com.maxa.device.companion

import android.content.Context
import java.io.File
import java.io.RandomAccessFile

class BlobStore(context: Context) {
    data class Blob(val id: String, val owner: String, val capability: String, val file: File, val expected: Long, var written: Long, var touched: Long)
    private val dir = File(context.filesDir, "blobs").apply { mkdirs(); listFiles()?.forEach { it.delete() } }
    private val entries = mutableMapOf<String, Blob>()
    private fun sweep() { val cutoff = System.currentTimeMillis() - 30 * 60_000; entries.values.filter { it.touched < cutoff }.toList().forEach { remove(it.id) } }
    @Synchronized fun create(owner: String, capability: String, size: Long): Blob {
        sweep()
        if (size !in 0..536870912L || entries.size >= 32 || entries.values.sumOf { it.expected } + size > 1073741824L) fail("INVALID_ARGUMENT", "Blob quota exceeded (512 MiB/file, 1 GiB total, 32 files)")
        val id = Protocol.hex(Protocol.randomBytes(16)); val file = File(dir, id); check(file.createNewFile())
        file.setReadable(false, false); file.setWritable(false, false); file.setReadable(true, true); file.setWritable(true, true)
        return Blob(id, owner, capability, file, size, 0, System.currentTimeMillis()).also { entries[id] = it }
    }
    @Synchronized fun fromBytes(owner: String, capability: String, bytes: ByteArray): Blob = create(owner, capability, bytes.size.toLong()).also { it.file.writeBytes(bytes); it.written = bytes.size.toLong() }
    @Synchronized fun fromFile(owner: String, capability: String, file: File): Blob = create(owner, capability, file.length()).also { blob -> file.copyTo(blob.file, true); blob.written = blob.expected; file.delete() }
    @Synchronized fun get(id: String, owner: String): Blob {
        sweep(); val b = entries[id] ?: fail("NOT_FOUND", "Blob expired or missing")
        if (b.owner != owner) fail("NOT_FOUND", "Blob not owned by session")
        b.touched = System.currentTimeMillis(); return b
    }
    @Synchronized fun write(owner: String, id: String, offset: Long, bytes: ByteArray) {
        val b = get(id, owner)
        if (b.capability != "install" || offset != b.written || bytes.size > 1048576 || offset + bytes.size > b.expected) fail("INVALID_ARGUMENT", "Blob writes must be sequential and within declared size")
        RandomAccessFile(b.file, "rw").use { it.seek(offset); it.write(bytes) }; b.written += bytes.size
    }
    data class Read(val bytes: ByteArray, val eof: Boolean, val id: String)
    @Synchronized fun read(owner: String, id: String, offset: Long, length: Int): Read {
        val b = get(id, owner)
        if (length !in 1..1048576 || offset !in 0..b.written || b.written != b.expected) fail("INVALID_ARGUMENT", "Invalid blob read or incomplete upload")
        val bytes = ByteArray(minOf(length.toLong(), b.written - offset).toInt())
        RandomAccessFile(b.file, "r").use { it.seek(offset); it.readFully(bytes) }
        return Read(bytes, offset + bytes.size == b.written, id)
    }
    @Synchronized fun remove(id: String) { entries.remove(id)?.file?.delete() }
    @Synchronized fun removeOwner(owner: String) { entries.values.filter { it.owner == owner }.map { it.id }.forEach { remove(it) } }
    @Synchronized fun clear() { entries.keys.toList().forEach { remove(it) } }
    fun cleanup() { synchronized(this) { sweep() } }
}
