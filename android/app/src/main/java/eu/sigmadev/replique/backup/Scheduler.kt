package eu.sigmadev.replique.backup

import android.content.Context
import androidx.work.Constraints
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.workDataOf
import eu.sigmadev.replique.proto.Conditions
import eu.sigmadev.replique.proto.DeviceConfig
import java.time.Duration
import java.util.concurrent.TimeUnit

/**
 * The WorkManager side: one periodic job with the phone's conditions, a
 * one-off job for "Back up now", and a continuation after a run stopped at
 * Android's daily limit.
 */
object Scheduler {
    const val SCHEDULED = "replique-scheduled"
    const val NOW = "replique-now"
    const val CONTINUE = "replique-continue"

    fun constraints(conditions: Conditions): Constraints = Constraints.Builder()
        .setRequiredNetworkType(if (conditions.unmeteredOnly) NetworkType.UNMETERED else NetworkType.CONNECTED)
        .setRequiresCharging(conditions.chargingOnly)
        .build()

    /**
     * (Re)schedules the periodic backup. UPDATE keeps the current period's
     * timing when only the constraints changed, so editing a rule on the
     * website does not trigger an immediate run.
     */
    fun schedule(context: Context, config: DeviceConfig, initialDelay: Duration = Duration.ZERO) {
        val daily = DailyTime.parse(config.dailyAt)
        if (daily != null) {
            // A time of day: the first run at the next such time, then every
            // 24 hours. Enqueued afresh each time, because UPDATE keeps the old
            // period's start and the time may just have changed.
            val request = PeriodicWorkRequestBuilder<BackupWorker>(24, TimeUnit.HOURS)
                .setConstraints(constraints(config.conditions))
                .setInitialDelay(DailyTime.delayUntil(daily).toMillis(), TimeUnit.MILLISECONDS)
                .addTag(SCHEDULED)
                .build()
            WorkManager.getInstance(context)
                .enqueueUniquePeriodicWork(SCHEDULED, ExistingPeriodicWorkPolicy.CANCEL_AND_REENQUEUE, request)
            return
        }
        val hours = config.intervalHours.coerceAtLeast(1).toLong()
        val request = PeriodicWorkRequestBuilder<BackupWorker>(hours, TimeUnit.HOURS)
            .setConstraints(constraints(config.conditions))
            .setInitialDelay(initialDelay.toMillis(), TimeUnit.MILLISECONDS)
            .addTag(SCHEDULED)
            .build()
        WorkManager.getInstance(context).enqueueUniquePeriodicWork(SCHEDULED, ExistingPeriodicWorkPolicy.UPDATE, request)
    }

    /**
     * Schedules the periodic backup unless it already is, without touching
     * an existing schedule. Called once the app can read shared storage:
     * scheduling earlier would start a run that can only fail.
     */
    fun ensureScheduled(context: Context, config: DeviceConfig) {
        val daily = DailyTime.parse(config.dailyAt)
        val hours = if (daily != null) 24L else config.intervalHours.coerceAtLeast(1).toLong()
        val request = PeriodicWorkRequestBuilder<BackupWorker>(hours, TimeUnit.HOURS)
            .setConstraints(constraints(config.conditions))
            .apply { if (daily != null) setInitialDelay(DailyTime.delayUntil(daily).toMillis(), TimeUnit.MILLISECONDS) }
            .addTag(SCHEDULED)
            .build()
        WorkManager.getInstance(context).enqueueUniquePeriodicWork(SCHEDULED, ExistingPeriodicWorkPolicy.KEEP, request)
    }

    /** "Back up now": no conditions (the protocol applies them to scheduled runs only). */
    fun backupNow(context: Context) {
        val request = OneTimeWorkRequestBuilder<BackupWorker>()
            .setInputData(workDataOf(BackupWorker.KEY_MANUAL to true))
            .addTag(NOW)
            .build()
        WorkManager.getInstance(context).enqueueUniqueWork(NOW, ExistingWorkPolicy.KEEP, request)
    }

    /** Continues a long first backup once Android's allowance is back, under the usual conditions. */
    fun continueAfter(context: Context, delay: Duration, conditions: Conditions) {
        val request = OneTimeWorkRequestBuilder<BackupWorker>()
            .setInitialDelay(delay.toMillis().coerceAtLeast(0), TimeUnit.MILLISECONDS)
            .setConstraints(constraints(conditions))
            .addTag(CONTINUE)
            .build()
        WorkManager.getInstance(context).enqueueUniqueWork(CONTINUE, ExistingWorkPolicy.REPLACE, request)
    }

    /** Stops whatever runs now (the Home screen's Stop button). */
    fun stopRunning(context: Context, config: DeviceConfig) {
        val wm = WorkManager.getInstance(context)
        wm.cancelUniqueWork(NOW)
        wm.cancelUniqueWork(CONTINUE)
        // A periodic run can only be stopped by cancelling the periodic work
        // itself; it is scheduled again right away, a full period from now,
        // so that it does not start over the moment the user stopped it.
        wm.cancelUniqueWork(SCHEDULED)
        // With a daily time, `schedule` already waits for the next one.
        schedule(context, config, Duration.ofHours(config.intervalHours.coerceAtLeast(1).toLong()))
    }

    fun cancelAll(context: Context) {
        val wm = WorkManager.getInstance(context)
        listOf(SCHEDULED, NOW, CONTINUE).forEach { wm.cancelUniqueWork(it) }
    }
}
