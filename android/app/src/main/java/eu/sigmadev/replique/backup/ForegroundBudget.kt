package eu.sigmadev.replique.backup

import eu.sigmadev.replique.data.Interval
import java.time.Duration
import java.time.Instant

/**
 * Android 15 lets an app run `dataSync` foreground services for about 6 hours
 * per 24 hours, then calls `Service.onTimeout` and gives it seconds to stop
 * before crashing it. A first backup of a phone easily takes longer, so the app
 * keeps its own account of the time used and stops restic cleanly a little
 * before the system would. restic writes an index of what it uploaded at least
 * every ten minutes during a backup, so the next run skips everything but the
 * last few minutes' worth and picks up from there. The system resets
 * the allowance when the user opens the app, and so does this account.
 */
object ForegroundBudget {
    val LIMIT: Duration = Duration.ofHours(6)
    val WINDOW: Duration = Duration.ofHours(24)

    /** Stop this long before the limit, to leave time to stop restic and report. */
    val MARGIN: Duration = Duration.ofMinutes(10)

    /** Below this, starting a run is not worth it. */
    val MINIMUM_RUN: Duration = Duration.ofMinutes(15)

    fun used(usage: List<Interval>, now: Instant): Duration {
        val from = now.minus(WINDOW)
        return usage.fold(Duration.ZERO) { total, i ->
            val start = maxOf(i.start, from)
            val end = minOf(i.end, now)
            if (end.isAfter(start)) total.plus(Duration.between(start, end)) else total
        }
    }

    /** Time left before the backup must stop, after the margin. Never negative. */
    fun remaining(usage: List<Interval>, now: Instant): Duration {
        val left = LIMIT.minus(used(usage, now)).minus(MARGIN)
        return if (left.isNegative) Duration.ZERO else left
    }

    /**
     * The earliest time at which [needed] will be available again, assuming
     * nothing else runs meanwhile: old intervals age out of the 24-hour window.
     */
    fun availableAt(usage: List<Interval>, now: Instant, needed: Duration = MINIMUM_RUN): Instant {
        if (remaining(usage, now) >= needed) return now
        // The allowance only grows when the start of the window passes over
        // used time, so it is enough to try the moments each interval ends
        // leaving the window, plus the moment that frees exactly what is needed.
        val candidates = usage.flatMap { listOf(it.start.plus(WINDOW), it.end.plus(WINDOW)) }
            .filter { it.isAfter(now) }
            .sorted()
        var previous = now
        for (t in candidates) {
            if (remaining(usage, t) >= needed) {
                // Narrow down between the previous candidate and this one.
                var lo = previous
                var hi = t
                while (Duration.between(lo, hi) > Duration.ofMinutes(1)) {
                    val mid = lo.plus(Duration.between(lo, hi).dividedBy(2))
                    if (remaining(usage, mid) >= needed) hi = mid else lo = mid
                }
                return hi
            }
            previous = t
        }
        return now.plus(WINDOW)
    }

    /** Drops what no longer counts, and merges the run that just happened. */
    fun record(usage: List<Interval>, run: Interval, now: Instant): List<Interval> =
        (usage.filter { it.end.isAfter(now.minus(WINDOW)) && it.start != run.start } + run).sortedBy { it.start }
}
