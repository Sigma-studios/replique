package eu.sigmadev.replique.backup

import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.os.BatteryManager
import android.os.Build
import androidx.work.CoroutineWorker
import androidx.work.ForegroundInfo
import androidx.work.WorkerParameters
import eu.sigmadev.replique.RepliqueApp
import eu.sigmadev.replique.data.Interval
import eu.sigmadev.replique.proto.RunStatus
import java.time.Duration
import java.time.Instant
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch
import kotlinx.coroutines.coroutineScope

/**
 * Runs one backup (and its check) as a WorkManager job promoted to a
 * `dataSync` foreground service, with the progress in its notification.
 */
class BackupWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    private val app get() = applicationContext as RepliqueApp

    override suspend fun doWork(): Result {
        val store = app.store
        if (store.current.pairing == null) return Result.success()
        val manual = inputData.getBoolean(KEY_MANUAL, false)
        val config = store.current.effectiveConfig
        if (!manual && !batteryAllows(config.conditions.minBatteryPercent)) return Result.success()

        val now = Instant.now()
        val budgetApplies = Build.VERSION.SDK_INT >= 35
        val remaining = if (budgetApplies) ForegroundBudget.remaining(store.current.foregroundUsage, now) else null
        if (remaining != null && remaining < ForegroundBudget.MINIMUM_RUN) {
            val at = ForegroundBudget.availableAt(store.current.foregroundUsage, now)
            Scheduler.continueAfter(applicationContext, Duration.between(now, at), config.conditions)
            return Result.success()
        }

        try {
            setForeground(foregroundInfo(LiveRun(running = true, phase = "Préparation…")))
        } catch (e: Exception) {
            // Android 12+ refuses to start a foreground service from the
            // background unless the app is exempted from battery optimisation
            // (ForegroundServiceStartNotAllowedException), and Android 15
            // refuses once the daily dataSync allowance is used up.
            Notifications.problem(
                applicationContext,
                "Réplique n'a pas pu démarrer la sauvegarde",
                "Android a refusé de la lancer en arrière-plan. Exemptez Réplique de l'optimisation de la batterie (Réglages de l'application).",
            )
            return Result.retry()
        }

        val start = Instant.now()
        val deadline = remaining?.let { start.plus(it) }
        val outcome = try {
            coroutineScope {
                // The notification follows the live state; the foreground time
                // used is written down as it goes, in case the process dies.
                val updater = launch {
                    var lastSaved = start
                    app.live.collect { live ->
                        if (live.running) notify(live)
                        val t = Instant.now()
                        if (Duration.between(lastSaved, t) > Duration.ofMinutes(1)) {
                            lastSaved = t
                            recordUsage(start, t)
                        }
                    }
                }
                try {
                    app.backupRunner().run(deadline = deadline, canReadStorage = app.canReadSharedStorage())
                } finally {
                    updater.cancel()
                }
            }
        } catch (e: CancellationException) {
            recordUsage(start, Instant.now())
            throw e
        }
        recordUsage(start, Instant.now())

        if (outcome.stoppedAtDeadline) {
            val t = Instant.now()
            val at = ForegroundBudget.availableAt(store.current.foregroundUsage, t)
            Scheduler.continueAfter(applicationContext, Duration.between(t, at), config.conditions)
        }
        outcome.backup?.let { backup ->
            if (backup.status !in setOf(RunStatus.Success, RunStatus.Partial, RunStatus.Interrupted)) {
                Notifications.problem(
                    applicationContext,
                    "La sauvegarde a échoué",
                    backup.message ?: "restic a terminé avec le statut ${backup.status.wire}.",
                )
            }
        }
        return Result.success()
    }

    override suspend fun getForegroundInfo(): ForegroundInfo = foregroundInfo(app.live.value)

    private fun foregroundInfo(live: LiveRun): ForegroundInfo {
        val notification = Notifications.progress(applicationContext, live)
        return if (Build.VERSION.SDK_INT >= 29) {
            ForegroundInfo(Notifications.ID_PROGRESS, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            ForegroundInfo(Notifications.ID_PROGRESS, notification)
        }
    }

    private fun notify(live: LiveRun) {
        applicationContext.getSystemService(NotificationManager::class.java)
            .notify(Notifications.ID_PROGRESS, Notifications.progress(applicationContext, live))
    }

    private fun recordUsage(start: Instant, end: Instant) {
        app.store.update { it.copy(foregroundUsage = ForegroundBudget.record(it.foregroundUsage, Interval(start, end), end)) }
    }

    private fun batteryAllows(minPercent: Int?): Boolean {
        if (minPercent == null) return true
        val status = applicationContext.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED)) ?: return true
        val plugged = status.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) != 0
        val level = status.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
        val scale = status.getIntExtra(BatteryManager.EXTRA_SCALE, 100)
        if (plugged || level < 0) return true
        return level * 100 / scale >= minPercent
    }

    companion object {
        const val KEY_MANUAL = "manual"
    }
}
