package eu.sigmadev.replique.backup

import eu.sigmadev.replique.api.ApiResult
import eu.sigmadev.replique.api.RepliqueApi
import eu.sigmadev.replique.api.Spool
import eu.sigmadev.replique.api.SpoolEntry
import eu.sigmadev.replique.data.Pairing
import eu.sigmadev.replique.data.RunRecord
import eu.sigmadev.replique.data.SecretStore
import eu.sigmadev.replique.data.Secrets
import eu.sigmadev.replique.data.Store
import eu.sigmadev.replique.proto.Api
import eu.sigmadev.replique.proto.BackupSummary
import eu.sigmadev.replique.proto.CheckSummary
import eu.sigmadev.replique.proto.DeviceConfig
import eu.sigmadev.replique.proto.FinishRun
import eu.sigmadev.replique.proto.JobKind
import eu.sigmadev.replique.proto.JobRules
import eu.sigmadev.replique.proto.JobSpec
import eu.sigmadev.replique.proto.Progress
import eu.sigmadev.replique.proto.Retention
import eu.sigmadev.replique.proto.RunError
import eu.sigmadev.replique.proto.RunKind
import eu.sigmadev.replique.proto.RunStatus
import eu.sigmadev.replique.proto.StartRun
import eu.sigmadev.replique.restic.BackupPlan
import eu.sigmadev.replique.restic.RepositoryAccess
import eu.sigmadev.replique.restic.Restic
import eu.sigmadev.replique.restic.ResticMessage
import eu.sigmadev.replique.restic.ResticPaths
import eu.sigmadev.replique.restic.ResticResult
import java.io.File
import java.time.Duration
import java.time.Instant
import java.util.UUID
import java.util.concurrent.atomic.AtomicLong
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull

/** How a call to [BackupRunner.run] ended. */
data class Outcome(
    val backup: RunRecord? = null,
    val check: RunRecord? = null,
    /** Set when nothing ran at all (not paired, another run in progress). */
    val skipped: String? = null,
    /** The backup hit [BackupRunner.run]'s deadline: schedule a continuation. */
    val stoppedAtDeadline: Boolean = false,
)

/**
 * One backup of the phone, as the protocol describes it: refresh the rules,
 * report a `backup` run (start, progress at most every 10 s, finish with
 * restic's summary and errors), then a `check` run listing the snapshot ids it
 * verified, which is what lets the server call them "verified on Hetzner".
 * Never `forget`: the phone's repository is `never-prune`.
 */
class BackupRunner(
    private val store: Store,
    private val secretStore: SecretStore,
    private val spool: Spool,
    private val paths: ResticPaths,
    private val apiFor: (Pairing, Secrets) -> RepliqueApi,
    private val live: LiveRunFlow,
    private val clientVersion: String,
    private val resticVersion: String,
    private val clock: () -> Instant = Instant::now,
    private val onConfigChanged: (DeviceConfig) -> Unit = {},
) {
    /**
     * Runs a backup then a check. [deadline] is when the backup must stop
     * (Android 15's daily allowance for dataSync services); it is then reported
     * as `interrupted` and the next run continues from what was uploaded.
     * [canReadStorage] is false without "All files access": the run is then
     * reported as failed with that reason rather than backing up nothing.
     */
    suspend fun run(deadline: Instant? = null, canReadStorage: Boolean = true): Outcome {
        if (!mutex.tryLock()) return Outcome(skipped = "Une sauvegarde est déjà en cours.")
        try {
            val pairing = store.current.pairing ?: return Outcome(skipped = "Not paired")
            val secrets = secretStore.load() ?: return Outcome(skipped = "Not paired")
            val api = apiFor(pairing, secrets)
            live.value = LiveRun(running = true, kind = RunKind.Backup, phase = "Préparation…", startedAt = clock())
            io { spool.flush(api) }
            val config = refreshConfig(api)
            val restic = Restic(paths, RepositoryAccess(pairing.storage, secrets.sshPrivateKey, secrets.resticPassword), pairing.resticHost)

            val (backup, atDeadline) = backup(api, restic, config, deadline, canReadStorage)
            var check: RunRecord? = null
            if (backup.status == RunStatus.Success || backup.status == RunStatus.Partial) {
                check = check(api, restic, config)
            }
            refreshSafeToDelete(api, pairing)
            return Outcome(backup = backup, check = check, stoppedAtDeadline = atDeadline)
        } finally {
            live.value = LiveRun()
            mutex.unlock()
        }
    }

    /**
     * Fetches the phone's rules if they changed (ETag), declares the job with
     * them, and returns the rules in force. Unreachable server: the last rules
     * received (or the defaults) still apply.
     */
    suspend fun refreshConfig(api: RepliqueApi): DeviceConfig {
        val before = store.current
        when (val reply = io { api.deviceConfig(before.configEtag.takeIf { before.config != null }) }) {
            is ApiResult.Ok -> {
                store.update { it.copy(config = reply.value, configEtag = reply.etag) }
                // New interval or conditions from the website: the schedule follows.
                if (reply.value != before.config) onConfigChanged(reply.value)
            }
            else -> Unit
        }
        val config = store.current.effectiveConfig
        io { api.putJob(config.job, jobSpec(config)) }
        return config
    }

    private suspend fun backup(
        api: RepliqueApi,
        restic: Restic,
        config: DeviceConfig,
        deadline: Instant?,
        canReadStorage: Boolean,
    ): Pair<RunRecord, Boolean> = coroutineScope {
        val clientRunId = UUID.randomUUID().toString()
        val startedAt = clock()
        report(api, SpoolEntry.Start(clientRunId, startRun(clientRunId, config.job, RunKind.Backup, startedAt)))

        val errors = mutableListOf<RunError>()
        var errorCount = 0L
        val unreadable = mutableListOf<String>()
        var summary: BackupSummary? = null
        val lastProgressSent = AtomicLong(0)
        var atDeadline = false

        fun finishWith(status: RunStatus, exitCode: Int, message: String?) = FinishRun(
            status = status,
            exitCode = exitCode,
            finishedAt = clock(),
            summary = summary,
            errors = errors.toList(),
            errorCount = errorCount,
            unreadablePaths = unreadable.toList(),
            message = message,
        )

        val finish: FinishRun = try {
            precheck(restic, config, canReadStorage)?.let { (code, message) ->
                return@coroutineScope record(api, clientRunId, RunKind.Backup, startedAt, finishWith(RunStatus.fromResticExit(code), code, message)) to false
            }
            val plan = BackupPlan.from(config)
            if (plan.paths.isEmpty()) {
                return@coroutineScope record(
                    api, clientRunId, RunKind.Backup, startedAt,
                    finishWith(RunStatus.Failed, 1, "Aucun dossier à sauvegarder n'existe sur ce téléphone."),
                ) to false
            }
            val args = backupArgs(plan, restic)
            live.value = live.value.copy(phase = "Analyse des fichiers…")
            val onMessage: (ResticMessage) -> Unit = { message ->
                when (message) {
                    is ResticMessage.Status -> {
                        val progress = message.status.toProgress(clock())
                        live.value = live.value.copy(progress = progress, phase = null)
                        maybeSendProgress(api, clientRunId, progress, lastProgressSent)
                    }
                    is ResticMessage.Summary -> summary = message.summary
                    is ResticMessage.Error -> {
                        errorCount++
                        if (errors.size < Api.MAX_REPORTED_ERRORS) errors += message.error
                        message.error.item?.let { if (unreadable.size < 1000) unreadable += it }
                    }
                    else -> Unit
                }
            }
            val remaining = deadline?.let { Duration.between(clock(), it).toMillis().coerceAtLeast(0) }
            val result: ResticResult? = if (remaining == null) {
                restic.run(args, onMessage = onMessage)
            } else {
                withTimeoutOrNull(remaining) { restic.run(args, onMessage = onMessage) }
            }
            if (result == null) {
                atDeadline = true
                finishWith(RunStatus.Interrupted, Restic.EXIT_INTERRUPTED, DEADLINE_MESSAGE)
            } else {
                val status = RunStatus.fromResticExit(result.exitCode)
                val message = if (status == RunStatus.Success || status == RunStatus.Partial) null else result.reason()
                finishWith(status, result.exitCode, message)
            }
        } catch (e: CancellationException) {
            // Android stopped the work (its time limit, or the user's Stop
            // button). The finish goes to the spool only: there are a few
            // seconds left at most, not enough for the network.
            withContext(NonCancellable) {
                val finish = finishWith(RunStatus.Interrupted, Restic.EXIT_INTERRUPTED, STOPPED_MESSAGE)
                spool.enqueue(SpoolEntry.Finish(clientRunId, finish))
                saveRecord(clientRunId, RunKind.Backup, startedAt, finish)
            }
            throw e
        }
        record(api, clientRunId, RunKind.Backup, startedAt, finish) to atDeadline
    }

    /**
     * What must hold before restic runs: storage readable, repository present
     * (created on the first run if pairing could not). Returns the exit code
     * and reason to report when it does not hold.
     */
    private suspend fun precheck(restic: Restic, config: DeviceConfig, canReadStorage: Boolean): Pair<Int, String>? {
        if (!canReadStorage) return 1 to "L'accès à tous les fichiers n'est pas autorisé pour Réplique."
        if (store.current.pairing?.repositoryReady != true) {
            live.value = live.value.copy(phase = "Préparation du dépôt…")
            val failed = restic.ensureInitialized()
            if (failed != null) return failed.exitCode to (failed.reason() ?: "restic init failed")
            store.update { s -> s.copy(pairing = s.pairing?.copy(repositoryReady = true)) }
        }
        return null
    }

    private fun backupArgs(plan: BackupPlan, restic: Restic): List<String> {
        val dir = File(paths.cacheDir, "excludes").apply { mkdirs() }
        val exclude = File(dir, "exclude.txt").apply { writeText(plan.excludes.joinToString("\n", postfix = "\n")) }
        val iexclude = File(dir, "iexclude.txt").apply { writeText(plan.iexcludes.joinToString("\n", postfix = "\n")) }
        return buildList {
            add("backup"); add("--json")
            add("--tag"); add(TAG)
            add("--host"); add(store.current.pairing!!.resticHost)
            add("--exclude-file"); add(exclude.absolutePath)
            add("--iexclude-file"); add(iexclude.absolutePath)
            addAll(plan.paths)
        }
    }

    private suspend fun check(api: RepliqueApi, restic: Restic, config: DeviceConfig): RunRecord {
        val clientRunId = UUID.randomUUID().toString()
        val startedAt = clock()
        live.value = LiveRun(running = true, kind = RunKind.Check, phase = "Vérification du dépôt…", startedAt = startedAt)
        report(api, SpoolEntry.Start(clientRunId, startRun(clientRunId, config.job, RunKind.Check, startedAt)))
        try {
            // The ids are listed before the check: those are the snapshots the
            // check covers, and the server marks exactly them as verified.
            val (listing, snapshots) = restic.snapshots()
            if (listing.exitCode != 0) {
                val status = RunStatus.fromResticExit(listing.exitCode)
                return record(
                    api, clientRunId, RunKind.Check, startedAt,
                    FinishRun(status, listing.exitCode, clock(), message = listing.reason()),
                )
            }
            val errors = mutableListOf<RunError>()
            var errorCount = 0L
            var numErrors = 0L
            val result = restic.run(listOf("check", "--json")) { message ->
                when (message) {
                    is ResticMessage.CheckSummary -> numErrors = message.numErrors
                    is ResticMessage.Error -> {
                        errorCount++
                        if (errors.size < Api.MAX_REPORTED_ERRORS) errors += message.error
                    }
                    else -> Unit
                }
            }
            val status = RunStatus.fromResticExit(result.exitCode)
            return record(
                api, clientRunId, RunKind.Check, startedAt,
                FinishRun(
                    status = status,
                    exitCode = result.exitCode,
                    finishedAt = clock(),
                    check = CheckSummary(numErrors = numErrors, snapshotIds = snapshots.map { it.id }),
                    errors = errors,
                    errorCount = errorCount,
                    message = if (status == RunStatus.Success) null else result.reason(),
                ),
            )
        } catch (e: CancellationException) {
            withContext(NonCancellable) {
                val finish = FinishRun(RunStatus.Interrupted, Restic.EXIT_INTERRUPTED, clock(), message = STOPPED_MESSAGE)
                spool.enqueue(SpoolEntry.Finish(clientRunId, finish))
                saveRecord(clientRunId, RunKind.Check, startedAt, finish)
            }
            throw e
        }
    }

    private suspend fun refreshSafeToDelete(api: RepliqueApi, pairing: Pairing) {
        val reply = io { api.safeToDelete(pairing.deviceId, null) }
        if (reply is ApiResult.Ok) store.update { it.copy(safeToDelete = reply.value, safeToDeleteCheckedAt = clock()) }
    }

    private fun startRun(clientRunId: String, job: String, kind: RunKind, startedAt: Instant) = StartRun(
        clientRunId = clientRunId,
        job = job,
        kind = kind,
        startedAt = startedAt,
        resticVersion = resticVersion,
        clientVersion = clientVersion,
    )

    /** Spools the report, then tries to send everything pending. */
    private suspend fun report(api: RepliqueApi, entry: SpoolEntry) {
        spool.enqueue(entry)
        io { spool.flush(api) }
    }

    private suspend fun record(
        api: RepliqueApi,
        clientRunId: String,
        kind: RunKind,
        startedAt: Instant,
        finish: FinishRun,
    ): RunRecord {
        report(api, SpoolEntry.Finish(clientRunId, finish))
        return saveRecord(clientRunId, kind, startedAt, finish)
    }

    private fun saveRecord(clientRunId: String, kind: RunKind, startedAt: Instant, finish: FinishRun): RunRecord {
        val record = RunRecord(
            clientRunId = clientRunId,
            kind = kind,
            startedAt = startedAt,
            finishedAt = finish.finishedAt,
            status = finish.status,
            message = finish.message,
            snapshotId = finish.summary?.snapshotId,
            filesProcessed = finish.summary?.totalFilesProcessed ?: 0,
            bytesProcessed = finish.summary?.totalBytesProcessed ?: 0,
            bytesAdded = finish.summary?.dataAddedPacked ?: 0,
            errorCount = finish.errorCount,
            unreadablePaths = finish.unreadablePaths.take(20),
        )
        store.update { if (kind == RunKind.Check) it.copy(lastCheck = record) else it.copy(lastBackup = record) }
        return record
    }

    // Progress goes straight to the server, at most every 10 seconds, and only
    // once the run's start got through: without a run id there is nowhere to
    // send it, and stale progress is not worth spooling.
    private fun kotlinx.coroutines.CoroutineScope.maybeSendProgress(
        api: RepliqueApi,
        clientRunId: String,
        progress: Progress,
        lastSent: AtomicLong,
    ) {
        val now = System.nanoTime()
        val previous = lastSent.get()
        if (previous != 0L && now - previous < Api.PROGRESS_MIN_INTERVAL_SECS * 1_000_000_000L) return
        if (!lastSent.compareAndSet(previous, now)) return
        val runId = spool.runId(clientRunId) ?: return
        launch(Dispatchers.IO) { api.progress(runId, progress) }
    }

    private suspend fun <T> io(block: () -> T): T = withContext(Dispatchers.IO) { block() }

    companion object {
        /** One restic at a time: a scheduled run and "Back up now" share this. */
        private val mutex = Mutex()

        const val TAG = "replique"

        const val DEADLINE_MESSAGE =
            "Arrêtée à la limite quotidienne d'Android pour les transferts en arrière-plan (6 h par 24 h); la prochaine sauvegarde reprend où celle-ci s'est arrêtée."
        const val STOPPED_MESSAGE = "Arrêtée par Android ou depuis l'application; la prochaine sauvegarde reprend où celle-ci s'est arrêtée."

        /** The job the phone declares: its rules as the website shows them. */
        fun jobSpec(config: DeviceConfig) = JobSpec(
            kind = JobKind.Backup,
            schedule = "every ${config.intervalHours}h",
            expectedIntervalSecs = config.intervalHours * 3600L,
            rules = JobRules(
                paths = config.folders.map { it.path },
                excludes = config.excludes + config.folders.flatMap { it.excludes },
                excludeCaches = false,
                retention = Retention.NeverPrune,
            ),
        )
    }
}
