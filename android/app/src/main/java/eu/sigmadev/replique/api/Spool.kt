package eu.sigmadev.replique.api

import eu.sigmadev.replique.proto.FinishRun
import eu.sigmadev.replique.proto.StartRun
import eu.sigmadev.replique.proto.Wire
import java.io.File
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.decodeFromString
import kotlinx.serialization.encodeToString

@Serializable
sealed interface SpoolEntry {
    val clientRunId: String

    @Serializable
    @SerialName("start")
    data class Start(@SerialName("client_run_id") override val clientRunId: String, val body: StartRun) : SpoolEntry

    @Serializable
    @SerialName("finish")
    data class Finish(@SerialName("client_run_id") override val clientRunId: String, val body: FinishRun) : SpoolEntry
}

/**
 * Starts and finishes of runs, kept on disk until the server has them.
 *
 * Backups must not depend on Réplique being up: every start and finish is
 * written here first, then sent in order. A start replayed later is harmless
 * (the server maps a client_run_id to one run), and so is an identical finish.
 * Progress is never spooled: stale progress is useless.
 */
class Spool(private val dir: File) {
    private val runIdsFile = File(dir, "run-ids.json")
    private val json = Wire.json

    init {
        dir.mkdirs()
    }

    /** Things that went wrong in a way retrying cannot fix, for the Home screen. */
    var lastRejection: String? = null
        private set

    @Synchronized
    fun enqueue(entry: SpoolEntry) {
        val next = (entries().lastOrNull()?.first ?: 0L) + 1
        val file = File(dir, "%012d.json".format(next))
        val tmp = File(dir, "%012d.json.tmp".format(next))
        tmp.writeText(json.encodeToString(SpoolEntry.serializer(), entry))
        check(tmp.renameTo(file)) { "could not write $file" }
    }

    @Synchronized
    fun pending(): Int = entries().size

    /** Drops everything, run ids included (unpairing: the token is gone, nothing can be sent). */
    @Synchronized
    fun clear() {
        dir.listFiles()?.forEach { it.delete() }
    }

    /** The server's run id for a run whose start got through, if any. */
    @Synchronized
    fun runId(clientRunId: String): String? = runIds()[clientRunId]

    /**
     * Sends what is pending, oldest first. Stops at the first sign the server
     * is unreachable and leaves the rest for next time; drops an entry the
     * server rejects (a 4xx would be rejected again forever). Returns true when
     * the spool is empty afterwards.
     */
    @Synchronized
    fun flush(api: RepliqueApi): Boolean {
        for ((_, file) in entries()) {
            val entry = runCatching { json.decodeFromString(SpoolEntry.serializer(), file.readText()) }.getOrNull()
            if (entry == null) {
                file.delete()
                continue
            }
            val result: ApiResult<*> = when (entry) {
                is SpoolEntry.Start -> api.startRun(entry.body).also {
                    if (it is ApiResult.Ok) rememberRunId(entry.clientRunId, it.value.runId)
                }
                is SpoolEntry.Finish -> {
                    val runId = runIds()[entry.clientRunId]
                    if (runId == null) {
                        // Its start was rejected, so the server has no run to
                        // finish. Nothing more can be done with it.
                        lastRejection = "finish of ${entry.clientRunId} has no run on the server"
                        file.delete()
                        continue
                    }
                    api.finish(runId, entry.body)
                }
            }
            when (result) {
                is ApiResult.Unreachable -> return false
                is ApiResult.Rejected -> {
                    lastRejection = "HTTP ${result.status}: ${result.error?.message ?: result.error?.error ?: "rejected"}"
                    file.delete()
                }
                else -> file.delete()
            }
        }
        return true
    }

    private fun entries(): List<Pair<Long, File>> =
        (dir.listFiles() ?: emptyArray())
            .mapNotNull { f -> f.name.removeSuffix(".json").takeIf { f.name.endsWith(".json") }?.toLongOrNull()?.let { it to f } }
            .sortedBy { it.first }

    private fun runIds(): Map<String, String> =
        runCatching {
            json.decodeFromString<List<Pair<String, String>>>(runIdsFile.readText()).toMap()
        }.getOrDefault(emptyMap())

    private fun rememberRunId(clientRunId: String, runId: String) {
        // Only recent runs can still have a finish pending; keep the file small.
        val list = (runIds() + (clientRunId to runId)).toList().takeLast(200)
        val tmp = File(dir, "run-ids.json.tmp")
        tmp.writeText(json.encodeToString(list))
        check(tmp.renameTo(runIdsFile))
    }
}
