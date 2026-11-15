package eu.sigmadev.replique.backup

import android.net.TrafficStats
import android.os.Process

/**
 * What the phone really sends during a run, from Android's per-app network
 * counters. restic and rclone run as children of the app, under its uid, so
 * their traffic counts. restic's own `bytes_done` counts what it read,
 * unchanged files included, and says nothing about the upload.
 *
 * The rate is taken over at least [window] milliseconds, so a burst between
 * two status lines (restic prints one every two seconds) does not swing it.
 */
class UploadMeter(
    private val txBytes: () -> Long = { TrafficStats.getUidTxBytes(Process.myUid()) },
    private val nowMillis: () -> Long = System::currentTimeMillis,
    private val window: Long = 5_000,
) {
    data class Reading(val sent: Long, val bytesPerSec: Long?)

    private val start = txBytes()
    private var lastAt = nowMillis()
    private var lastTx = start
    private var rate: Long? = null

    /** Null where the device does not count per-app traffic (it then says UNSUPPORTED, -1). */
    fun read(): Reading? {
        val tx = txBytes()
        if (start < 0 || tx < 0) return null
        val now = nowMillis()
        val elapsed = now - lastAt
        if (elapsed >= window) {
            rate = ((tx - lastTx).coerceAtLeast(0) * 1000 / elapsed)
            lastAt = now
            lastTx = tx
        }
        return Reading((tx - start).coerceAtLeast(0), rate)
    }
}
