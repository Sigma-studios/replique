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

    fun createChannels(context: Context) {
        val nm = context.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_PROGRESS, "Sauvegarde en cours", NotificationManager.IMPORTANCE_LOW).apply {
                description = "Progression de la sauvegarde pendant qu'elle tourne."
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
