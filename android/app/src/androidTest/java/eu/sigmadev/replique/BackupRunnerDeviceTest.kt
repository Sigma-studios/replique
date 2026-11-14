package eu.sigmadev.replique

import androidx.test.ext.junit.runners.AndroidJUnit4
import eu.sigmadev.replique.api.ApiResult
import eu.sigmadev.replique.api.RepliqueApi
import eu.sigmadev.replique.api.Spool
import eu.sigmadev.replique.backup.BackupRunner
import eu.sigmadev.replique.backup.LiveRun
import eu.sigmadev.replique.data.Pairing
import eu.sigmadev.replique.data.SecretStore
import eu.sigmadev.replique.data.Secrets
import eu.sigmadev.replique.data.Store
import eu.sigmadev.replique.proto.Ack
import eu.sigmadev.replique.proto.DeviceConfig
import eu.sigmadev.replique.proto.FinishRun
import eu.sigmadev.replique.proto.FolderRule
import eu.sigmadev.replique.proto.JobSpec
import eu.sigmadev.replique.proto.PairRequest
import eu.sigmadev.replique.proto.PairResponse
import eu.sigmadev.replique.proto.Progress
import eu.sigmadev.replique.proto.Retention
import eu.sigmadev.replique.proto.RunKind
import eu.sigmadev.replique.proto.RunStatus
import eu.sigmadev.replique.proto.SafeToDelete
import eu.sigmadev.replique.proto.StartRun
import eu.sigmadev.replique.proto.StartRunReply
import java.io.File
import java.time.Instant
import java.util.UUID
import kotlin.random.Random
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

/** Réplique's server, in memory: answers like the real one and keeps what it was sent. */
private class RecordingServer(private val config: DeviceConfig) : RepliqueApi {
    @Volatile var up = true
    val starts = mutableListOf<StartRun>()
    val finishes = mutableListOf<Pair<String, FinishRun>>()
    val progress = mutableListOf<Pair<String, Progress>>()
    val jobs = mutableListOf<JobSpec>()
    private val runIds = mutableMapOf<String, String>()

    private fun <T> answer(value: () -> T): ApiResult<T> =
        if (up) synchronized(this) { ApiResult.Ok(value(), etag = "\"v1\"") } else ApiResult.Unreachable("down")

    override fun pair(request: PairRequest): ApiResult<PairResponse> = error("not used")
    override fun deviceConfig(etag: String?) = answer { config }
    override fun putJob(name: String, spec: JobSpec) = answer { jobs += spec; Ack(true) }
    override fun startRun(start: StartRun) = answer {
        starts += start
        StartRunReply(runIds.getOrPut(start.clientRunId) { "run_${runIds.size + 1}" })
    }
    override fun progress(runId: String, progress: Progress) = answer { this.progress += runId to progress; Ack(true) }
    override fun finish(runId: String, finish: FinishRun) = answer { finishes += runId to finish; Ack(true) }
    override fun safeToDelete(deviceId: String, since: Instant?) = answer { SafeToDelete() }
}

private class MemorySecrets(var secrets: Secrets?) : SecretStore {
    override fun load() = secrets
    override fun save(secrets: Secrets) { this.secrets = secrets }
    override fun clear() { secrets = null }
}

/**
 * One whole backup as the app runs it, on the emulator: real restic and
 * rclone against the test SFTP server, reports to an in-memory server.
 */
@RunWith(AndroidJUnit4::class)
class BackupRunnerDeviceTest {
    private lateinit var data: File
    private lateinit var work: File

    @Before
    fun setUp() {
        TestServer.grantAllFilesAccess()
        data = TestServer.dataDir()
        work = File(TestServer.context.cacheDir, "runner-test-${UUID.randomUUID()}").apply { mkdirs() }
    }

    @After
    fun tearDown() {
        data.deleteRecursively()
        work.deleteRecursively()
    }

    private fun runner(server: RecordingServer, store: Store, spool: Spool) = BackupRunner(
        store = store,
        secretStore = MemorySecrets(Secrets("rpl_test", "pw-" + UUID.randomUUID(), TestServer.privateKey, "ssh-ed25519 test")),
        spool = spool,
        paths = TestServer.paths(),
        apiFor = { _, _ -> server },
        live = MutableStateFlow(LiveRun()),
        clientVersion = "replique-android test",
        resticVersion = BuildConfig.RESTIC_VERSION,
    )

    private fun pairedStore(): Store = Store(File(work, "state.json")).apply {
        update {
            it.copy(
                pairing = Pairing(
                    server = "http://unused", deviceId = "dev_test", deviceName = "Émulateur",
                    storage = TestServer.storage(), resticHost = "emulateur", pairedAt = Instant.now(),
                ),
            )
        }
    }

    private fun config() = DeviceConfig.androidDefault().copy(folders = listOf(FolderRule(data.path)))

    @Test
    fun backupThenCheckAreReported(): Unit = runBlocking {
        File(data, "DCIM").mkdirs()
        File(data, "DCIM/IMG_0001.jpg").writeBytes(Random.nextBytes(1_500_000))
        File(data, "notes.txt").writeText("bonjour")
        val server = RecordingServer(config())
        val store = pairedStore()
        val outcome = runner(server, store, Spool(File(work, "spool"))).run()

        assertEquals(RunStatus.Success, outcome.backup!!.status)
        assertEquals(RunStatus.Success, outcome.check!!.status)
        // The repository did not exist: the first run created it.
        assertTrue(store.current.pairing!!.repositoryReady)
        // The job was declared with the phone's rules, never pruned.
        assertEquals(Retention.NeverPrune, server.jobs.single().rules.retention)
        assertEquals(listOf(data.path), server.jobs.single().rules.paths)

        assertEquals(listOf(RunKind.Backup, RunKind.Check), server.starts.map { it.kind })
        assertTrue(server.starts.all { it.job == "phone" && it.resticVersion == BuildConfig.RESTIC_VERSION })
        val (backupRun, backup) = server.finishes[0]
        val (checkRun, check) = server.finishes[1]
        assertEquals("run_1", backupRun)
        assertEquals("run_2", checkRun)
        assertEquals(RunStatus.Success, backup.status)
        assertEquals(2L, backup.summary!!.filesNew)
        val snapshotId = backup.summary!!.snapshotId!!
        // The check lists the snapshot it verified: what makes it "verified on Hetzner".
        assertEquals(listOf(snapshotId), check.check!!.snapshotIds)
        assertEquals(0L, check.check!!.numErrors)
        assertEquals(snapshotId, store.current.lastBackup!!.snapshotId)
    }

    @Test
    fun deadlineStopsTheBackupAsInterrupted(): Unit = runBlocking {
        repeat(8) { File(data, "video$it.mp4").writeBytes(Random.nextBytes(25_000_000)) }
        val server = RecordingServer(config())
        val store = pairedStore()
        val runner = runner(server, store, Spool(File(work, "spool")))
        // Creating the repository takes a second or two; 200 MB take longer
        // than the rest, so the deadline falls inside the backup.
        val outcome = runner.run(deadline = Instant.now().plusSeconds(6))

        assertTrue(outcome.stoppedAtDeadline)
        assertEquals(RunStatus.Interrupted, outcome.backup!!.status)
        assertNull("no check after an interrupted backup", outcome.check)
        val finish = server.finishes.single().second
        assertEquals(RunStatus.Interrupted, finish.status)
        assertEquals(130, finish.exitCode)
        assertEquals(BackupRunner.DEADLINE_MESSAGE, finish.message)
    }

    @Test
    fun offlineReportsAreSpooledAndSentLater(): Unit = runBlocking {
        File(data, "a.txt").writeText("a")
        val server = RecordingServer(config()).apply { up = false }
        val store = pairedStore()
        val spool = Spool(File(work, "spool"))
        val outcome = runner(server, store, spool).run()
        // The backup does not need the server.
        assertEquals(RunStatus.Success, outcome.backup!!.status)
        assertEquals(RunStatus.Success, outcome.check!!.status)
        assertEquals(4, spool.pending())
        assertTrue(server.starts.isEmpty())

        server.up = true
        assertTrue(spool.flush(server))
        assertEquals(listOf(RunKind.Backup, RunKind.Check), server.starts.map { it.kind })
        assertEquals(listOf("run_1", "run_2"), server.finishes.map { it.first })
        assertEquals(outcome.backup!!.snapshotId, server.finishes[1].second.check!!.snapshotIds.single())
    }
}
