package eu.sigmadev.replique

import eu.sigmadev.replique.api.ApiResult
import eu.sigmadev.replique.api.RepliqueApi
import eu.sigmadev.replique.api.Spool
import eu.sigmadev.replique.api.SpoolEntry
import eu.sigmadev.replique.proto.Ack
import eu.sigmadev.replique.proto.ApiError
import eu.sigmadev.replique.proto.DeviceConfig
import eu.sigmadev.replique.proto.FinishRun
import eu.sigmadev.replique.proto.JobSpec
import eu.sigmadev.replique.proto.PairRequest
import eu.sigmadev.replique.proto.PairResponse
import eu.sigmadev.replique.proto.Progress
import eu.sigmadev.replique.proto.RunKind
import eu.sigmadev.replique.proto.RunStatus
import eu.sigmadev.replique.proto.SafeToDelete
import eu.sigmadev.replique.proto.StartRun
import eu.sigmadev.replique.proto.StartRunReply
import java.nio.file.Files
import java.time.Instant
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** A server that is up or down at will, and records what it received. */
class FakeServer : RepliqueApi {
    var up = true
    var rejectStarts = false
    val calls = mutableListOf<String>()
    private val runIds = mutableMapOf<String, String>()

    private fun <T> answer(call: String, value: () -> T): ApiResult<T> {
        if (!up) return ApiResult.Unreachable("connection refused")
        calls += call
        return ApiResult.Ok(value())
    }

    override fun pair(request: PairRequest): ApiResult<PairResponse> = error("not used")
    override fun deviceConfig(etag: String?): ApiResult<DeviceConfig> =
        if (etag == "v1") ApiResult.NotModified else answer("config") { DeviceConfig.androidDefault() }
    override fun putJob(name: String, spec: JobSpec) = answer("job $name") { Ack(true) }
    override fun startRun(start: StartRun): ApiResult<StartRunReply> {
        if (up && rejectStarts) return ApiResult.Rejected(400, ApiError("bad_request", "nope"))
        // Like the server: one client_run_id is always one run.
        return answer("start ${start.clientRunId}") { StartRunReply(runIds.getOrPut(start.clientRunId) { "run_${runIds.size + 1}" }) }
    }
    override fun progress(runId: String, progress: Progress) = answer("progress $runId") { Ack(true) }
    override fun finish(runId: String, finish: FinishRun) = answer("finish $runId ${finish.status.wire}") { Ack(true) }
    override fun safeToDelete(deviceId: String, since: Instant?) = answer("safe") { SafeToDelete() }
}

class SpoolTest {
    private val now = Instant.parse("2026-11-14T03:12:45Z")
    private fun start(id: String) = SpoolEntry.Start(id, StartRun(id, "phone", RunKind.Backup, now))
    private fun finish(id: String, status: RunStatus = RunStatus.Success) =
        SpoolEntry.Finish(id, FinishRun(status, 0, now))

    @Test
    fun reportsWaitForTheServerAndGoOutInOrder() {
        val spool = Spool(Files.createTempDirectory("spool").toFile())
        val server = FakeServer().apply { up = false }
        spool.enqueue(start("a"))
        assertFalse(spool.flush(server))
        spool.enqueue(finish("a"))
        spool.enqueue(start("b"))
        spool.enqueue(finish("b", RunStatus.Interrupted))
        assertFalse(spool.flush(server))
        assertEquals(4, spool.pending())

        server.up = true
        assertTrue(spool.flush(server))
        assertEquals(0, spool.pending())
        assertEquals(listOf("start a", "finish run_1 success", "start b", "finish run_2 interrupted"), server.calls)
        assertEquals("run_1", spool.runId("a"))
    }

    @Test
    fun survivesARestart() {
        val dir = Files.createTempDirectory("spool").toFile()
        Spool(dir).apply {
            enqueue(start("a"))
            enqueue(finish("a"))
        }
        val server = FakeServer()
        assertTrue(Spool(dir).flush(server))
        assertEquals(listOf("start a", "finish run_1 success"), server.calls)
    }

    @Test
    fun rejectedReportsAreDroppedNotRetriedForever() {
        val spool = Spool(Files.createTempDirectory("spool").toFile())
        val server = FakeServer().apply { rejectStarts = true }
        spool.enqueue(start("a"))
        spool.enqueue(finish("a"))
        assertTrue(spool.flush(server))
        assertEquals(0, spool.pending())
        assertTrue(server.calls.isEmpty())
        assertTrue(spool.lastRejection != null)
    }

    @Test
    fun clearForgetsEverything() {
        val spool = Spool(Files.createTempDirectory("spool").toFile())
        spool.enqueue(start("a"))
        spool.clear()
        assertEquals(0, spool.pending())
        spool.enqueue(start("b"))
        assertEquals(1, spool.pending())
    }
}
