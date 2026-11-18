package eu.sigmadev.replique.backup

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import eu.sigmadev.replique.R
import eu.sigmadev.replique.proto.RunKind
import eu.sigmadev.replique.ui.Format
import eu.sigmadev.replique.ui.MainActivity

object Notifications {
    const val CHANNEL_PROGRESS = "backup-progress"
    const val CHANNEL_PROBLEMS = "backup-problems"
    const val ID_PROGRESS = 1
    const val ID_PROBLEM = 2
    const val ID_SKIPPED = 3
    const val ID_DONE = 4
    const val CHANNEL_SKIPPED = "backup-skipped"
    const val CHANNEL_DONE = "backup-done"

    fun createChannels(context: Context) {
        val nm = context.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_PROGRESS, "Sauvegarde en cours", NotificationManager.IMPORTANCE_LOW).apply {
                description = "Progression de la sauvegarde pendant qu'elle tourne."
                setShowBadge(false)
            },
        )
        // Low importance: no sound at 4 in the morning for a phone left off
        // its charger, or for a backup that went well.
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_SKIPPED, "Sauvegardes reportées", NotificationManager.IMPORTANCE_LOW).apply {
                description = "Une sauvegarde prévue n'a pas démarré, et pourquoi."
            },
        )
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_DONE, "Sauvegardes terminées", NotificationManager.IMPORTANCE_LOW).apply {
                description = "Ce que la dernière sauvegarde a envoyé."
                setShowBadge(false)
            },
        )
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_PROBLEMS, "Problèmes de sauvegarde", NotificationManager.IMPORTANCE_DEFAULT).apply {
                description = "Une sauvegarde a échoué ou n'a pas pu démarrer."
            },
        )
    }

    private fun openApp(context: Context): PendingIntent = PendingIntent.getActivity(
        context, 0,
        Intent(context, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    fun progress(context: Context, live: LiveRun) = NotificationCompat.Builder(context, CHANNEL_PROGRESS)
        .setSmallIcon(R.drawable.ic_notification)
        .setContentTitle(if (live.kind == RunKind.Check) "Réplique vérifie la sauvegarde" else "Réplique sauvegarde le téléphone")
        .setContentText(progressText(live))
        .setOngoing(true)
        .setOnlyAlertOnce(true)
        .setSilent(true)
        .setContentIntent(openApp(context))
        .setCategory(NotificationCompat.CATEGORY_PROGRESS)
        .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
        .apply {
            val p = live.progress
            if (p == null || live.kind == RunKind.Check) setProgress(0, 0, true)
            else setProgress(1000, (p.percentDone * 1000).toInt().coerceIn(0, 1000), false)
        }
        .build()

    private fun progressText(live: LiveRun): String {
        val p = live.progress ?: return live.phase ?: "Préparation…"
        val eta = p.secondsRemaining?.let { " · encore ${Format.duration(it)}" } ?: ""
        val rate = p.uploadBytesPerSec?.let { " · ↑ ${Format.bytes(it)}/s" } ?: ""
        return "${Format.percent(p.percentDone)} · ${Format.bytes(p.bytesDone)} sur ${Format.bytes(p.totalBytes)}$rate$eta"
    }

    fun skipped(context: Context, text: String) = post(context, ID_SKIPPED, CHANNEL_SKIPPED, "La sauvegarde n'a pas démarré", text)

    /** What a finished backup sent: the size of what changed since the last one. */
    fun done(context: Context, record: eu.sigmadev.replique.data.RunRecord) {
        val changes = buildList {
            val (n, m) = record.filesNew to record.filesChanged
            if (n > 0) add("${Format.count(n)} " + if (n > 1) "nouveaux fichiers" else "nouveau fichier")
            if (m > 0) add("${Format.count(m)} " + if (m > 1) "modifiés" else "modifié")
        }
        val text = if (record.bytesAdded == 0L && changes.isEmpty()) {
            "Rien de nouveau depuis la dernière sauvegarde."
        } else {
            "${Format.bytes(record.bytesAdded)} envoyés" + if (changes.isEmpty()) "." else " · ${changes.joinToString(", ")}."
        }
        post(context, ID_DONE, CHANNEL_DONE, "Sauvegarde terminée", text)
    }

    private fun post(context: Context, id: Int, channel: String, title: String, text: String) {
        context.getSystemService(NotificationManager::class.java).notify(
            id,
            NotificationCompat.Builder(context, channel)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle(title)
                .setContentText(text)
                .setStyle(NotificationCompat.BigTextStyle().bigText(text))
                .setContentIntent(openApp(context))
                .setAutoCancel(true)
                .build(),
        )
    }

    fun problem(context: Context, title: String, text: String) {
        val nm = context.getSystemService(NotificationManager::class.java)
        nm.notify(
            ID_PROBLEM,
            NotificationCompat.Builder(context, CHANNEL_PROBLEMS)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle(title)
                .setContentText(text)
                .setStyle(NotificationCompat.BigTextStyle().bigText(text))
                .setContentIntent(openApp(context))
                .setAutoCancel(true)
                .build(),
        )
    }
}
