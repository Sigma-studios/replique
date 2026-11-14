package eu.sigmadev.replique.ui

import eu.sigmadev.replique.proto.RunStatus
import java.time.Duration
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.Locale

/** Numbers and dates the way the screens show them, in French. */
object Format {
    private val locale: Locale = Locale.FRANCE

    fun bytes(n: Long): String {
        if (n < 1000) return "$n o"
        val units = listOf("Ko", "Mo", "Go", "To")
        var value = n.toDouble()
        var unit = -1
        while (value >= 1000 && unit < units.size - 1) {
            value /= 1000
            unit++
        }
        return if (value >= 100) "%.0f %s".format(locale, value, units[unit])
        else "%.1f %s".format(locale, value, units[unit])
    }

    fun percent(fraction: Double): String = "%.0f %%".format(locale, (fraction * 100).coerceIn(0.0, 100.0))

    fun count(n: Long): String = "%,d".format(locale, n)

    /** "1 fichier", "0 fichier", "2 fichiers": French keeps 0 and 1 singular. */
    fun files(n: Long): String = "${count(n)} fichier" + if (n > 1) "s" else ""

    fun duration(seconds: Long): String {
        val d = Duration.ofSeconds(seconds.coerceAtLeast(0))
        return when {
            d.toDays() > 0 -> "${d.toDays()} j ${d.toHours() % 24} h"
            d.toHours() > 0 -> "${d.toHours()} h %02d".format(d.toMinutes() % 60)
            d.toMinutes() > 0 -> "${d.toMinutes()} min"
            else -> "${d.seconds} s"
        }
    }

    private val dateTime = DateTimeFormatter.ofPattern("d MMM yyyy 'à' HH:mm", locale)
    private val time = DateTimeFormatter.ofPattern("HH:mm", locale)

    /** "aujourd'hui à 03:12", "hier à 23:10", or a full date. */
    fun moment(instant: Instant, now: Instant = Instant.now(), zone: ZoneId = ZoneId.systemDefault()): String {
        val day = instant.atZone(zone).toLocalDate()
        val today = now.atZone(zone).toLocalDate()
        val t = time.format(instant.atZone(zone))
        return when (day) {
            today -> "aujourd'hui à $t"
            today.minusDays(1) -> "hier à $t"
            today.plusDays(1) -> "demain à $t"
            else -> dateTime.format(instant.atZone(zone))
        }
    }

    fun status(status: RunStatus): String = when (status) {
        RunStatus.Running -> "en cours"
        RunStatus.Success -> "réussie"
        RunStatus.Partial -> "partielle (fichiers illisibles)"
        RunStatus.RepositoryMissing -> "dépôt introuvable"
        RunStatus.LockFailed -> "dépôt verrouillé"
        RunStatus.WrongPassword -> "mauvais mot de passe"
        RunStatus.Interrupted -> "interrompue"
        RunStatus.Failed -> "échouée"
        RunStatus.Lost -> "perdue"
    }
}
