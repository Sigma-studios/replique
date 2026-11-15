package eu.sigmadev.replique.backup

import java.time.Duration
import java.time.LocalTime
import java.time.ZonedDateTime

/** `daily_at` from the website's rules: `HH:MM` in the phone's own time zone. */
object DailyTime {
    fun parse(s: String?): LocalTime? {
        val m = Regex("""^(\d{2}):(\d{2})$""").matchEntire(s?.trim() ?: return null) ?: return null
        val (h, min) = m.destructured
        return if (h.toInt() < 24 && min.toInt() < 60) LocalTime.of(h.toInt(), min.toInt()) else null
    }

    /**
     * How long until the next [at] after [now]. A time just past (under a
     * minute) counts as tomorrow's, so rescheduling at 02:00:30 does not run
     * again right away.
     */
    fun delayUntil(at: LocalTime, now: ZonedDateTime = ZonedDateTime.now()): Duration {
        var next = now.with(at).withSecond(0).withNano(0)
        if (!next.isAfter(now.plusMinutes(1))) next = next.plusDays(1)
        return Duration.between(now, next)
    }
}
