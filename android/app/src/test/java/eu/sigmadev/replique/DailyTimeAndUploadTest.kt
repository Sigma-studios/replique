package eu.sigmadev.replique

import eu.sigmadev.replique.backup.DailyTime
import eu.sigmadev.replique.backup.UploadMeter
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import java.time.Duration
import java.time.LocalTime
import java.time.ZoneId
import java.time.ZonedDateTime

class DailyTimeAndUploadTest {
    private val paris = ZoneId.of("Europe/Paris")

    @Test
    fun parsesOnlyHoursAndMinutes() {
        assertEquals(LocalTime.of(2, 0), DailyTime.parse("02:00"))
        assertEquals(LocalTime.of(23, 59), DailyTime.parse(" 23:59 "))
        assertNull(DailyTime.parse("24:00"))
        assertNull(DailyTime.parse("2:00"))
        assertNull(DailyTime.parse(null))
    }

    @Test
    fun nextRunIsTonightOrTomorrow() {
        val evening = ZonedDateTime.of(2026, 11, 15, 22, 30, 0, 0, paris)
        assertEquals(Duration.ofMinutes(210), DailyTime.delayUntil(LocalTime.of(2, 0), evening))
        val morning = ZonedDateTime.of(2026, 11, 15, 1, 0, 0, 0, paris)
        assertEquals(Duration.ofHours(1), DailyTime.delayUntil(LocalTime.of(2, 0), morning))
        // Rescheduled right at the time (a run just started): tomorrow's, not now.
        val atTime = ZonedDateTime.of(2026, 11, 15, 2, 0, 20, 0, paris)
        assertEquals(Duration.ofHours(24).minusSeconds(20), DailyTime.delayUntil(LocalTime.of(2, 0), atTime))
    }

    @Test
    fun uploadRateOverAWindow() {
        var tx = 1_000_000L
        var now = 0L
        val meter = UploadMeter(txBytes = { tx }, nowMillis = { now }, window = 5_000)
        tx += 2_000_000; now += 2_000
        assertEquals(UploadMeter.Reading(2_000_000, null), meter.read())
        tx += 3_000_000; now += 3_000
        // 5 MB over 5 s.
        assertEquals(UploadMeter.Reading(5_000_000, 1_000_000), meter.read())
        tx += 500_000; now += 1_000
        assertEquals(UploadMeter.Reading(5_500_000, 1_000_000), meter.read())
    }

    @Test
    fun noCountersNoReading() {
        val meter = UploadMeter(txBytes = { -1L }, nowMillis = { 0L })
        assertNull(meter.read())
    }
}
