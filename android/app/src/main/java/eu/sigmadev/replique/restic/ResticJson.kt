package eu.sigmadev.replique.restic

import eu.sigmadev.replique.proto.BackupSummary
import eu.sigmadev.replique.proto.InstantSerializer
import eu.sigmadev.replique.proto.Progress
import eu.sigmadev.replique.proto.RunError
import eu.sigmadev.replique.proto.Wire
import java.time.Instant
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.decodeFromJsonElement
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive

/** One line of `restic … --json` output, on stdout or stderr. */
sealed interface ResticMessage {
    /** `backup`'s periodic `status` (only when RESTIC_PROGRESS_FPS is set: restic has no terminal here). */
    data class Status(val status: ResticStatus) : ResticMessage

    /** `backup`'s final `summary`. */
    data class Summary(val summary: BackupSummary) : ResticMessage

    /** `check`'s final `summary`. */
    data class CheckSummary(val numErrors: Long, val brokenPacks: List<String>) : ResticMessage

    /** A file restic could not read (`backup`) or a problem `check` found. */
    data class Error(val error: RunError) : ResticMessage

    /** Why restic is about to exit with a non-zero code (restic 0.17+). */
    data class ExitError(val code: Int, val message: String) : ResticMessage

    /** Messages this app has no use for (`verbose_status`, `initialized`, …). */
    data class Other(val type: String) : ResticMessage
}

@Serializable
data class ResticStatus(
    @SerialName("seconds_elapsed") val secondsElapsed: Long = 0,
    @SerialName("seconds_remaining") val secondsRemaining: Long? = null,
    @SerialName("percent_done") val percentDone: Double = 0.0,
    @SerialName("total_files") val totalFiles: Long = 0,
    @SerialName("files_done") val filesDone: Long = 0,
    @SerialName("total_bytes") val totalBytes: Long = 0,
    @SerialName("bytes_done") val bytesDone: Long = 0,
    @SerialName("error_count") val errorCount: Long = 0,
    @SerialName("current_files") val currentFiles: List<String> = emptyList(),
) {
    fun toProgress(at: Instant) = Progress(
        at = at,
        percentDone = percentDone,
        secondsElapsed = secondsElapsed,
        secondsRemaining = secondsRemaining,
        totalFiles = totalFiles,
        filesDone = filesDone,
        totalBytes = totalBytes,
        bytesDone = bytesDone,
        errorCount = errorCount,
        currentFiles = currentFiles,
    )
}

/** An entry of `restic snapshots --json`. */
@Serializable
data class ResticSnapshot(
    val id: String,
    @SerialName("short_id") val shortId: String? = null,
    @Serializable(with = InstantSerializer::class) val time: Instant,
    val paths: List<String> = emptyList(),
    val hostname: String? = null,
    val tags: List<String> = emptyList(),
)

/** A `node` line of `restic ls --json`. */
@Serializable
data class ResticNode(
    val name: String,
    val type: String,
    val path: String,
    val size: Long = 0,
    @Serializable(with = InstantSerializer::class) val mtime: Instant? = null,
)

object ResticJson {
    private val json = Wire.json

    /** Parses one output line; null for anything that is not a restic JSON message. */
    fun parseLine(line: String): ResticMessage? {
        val trimmed = line.trim()
        if (!trimmed.startsWith("{")) return null
        val obj = runCatching { json.parseToJsonElement(trimmed).jsonObject }.getOrNull() ?: return null
        val type = obj.string("message_type") ?: return null
        return runCatching {
            when (type) {
                "status" -> ResticMessage.Status(json.decodeFromJsonElement<ResticStatus>(obj))
                "summary" ->
                    if ("num_errors" in obj) {
                        ResticMessage.CheckSummary(
                            numErrors = obj["num_errors"]?.jsonPrimitive?.contentOrNull?.toLongOrNull() ?: 0,
                            brokenPacks = (obj["broken_packs"] as? kotlinx.serialization.json.JsonArray)
                                ?.mapNotNull { (it as? JsonPrimitive)?.contentOrNull } ?: emptyList(),
                        )
                    } else {
                        ResticMessage.Summary(json.decodeFromJsonElement<BackupSummary>(obj))
                    }
                "error" -> ResticMessage.Error(
                    RunError(
                        message = errorText(obj["error"]) ?: obj.string("message") ?: "unknown error",
                        during = obj.string("during")?.takeIf { it.isNotEmpty() },
                        item = obj.string("item")?.takeIf { it.isNotEmpty() },
                    ),
                )
                "exit_error" -> ResticMessage.ExitError(
                    code = obj.string("code")?.toIntOrNull() ?: -1,
                    message = obj.string("message") ?: "",
                )
                else -> ResticMessage.Other(type)
            }
        }.getOrElse { ResticMessage.Other(type) }
    }

    fun snapshots(output: String): List<ResticSnapshot> = json.decodeFromString(output)

    /** The `node` lines of `restic ls --json`; the first line describes the snapshot and is skipped. */
    fun lsNode(line: String): ResticNode? {
        val trimmed = line.trim()
        if (!trimmed.startsWith("{")) return null
        val obj = runCatching { json.parseToJsonElement(trimmed).jsonObject }.getOrNull() ?: return null
        val kind = obj.string("message_type") ?: obj.string("struct_type")
        if (kind != "node") return null
        return runCatching { json.decodeFromJsonElement<ResticNode>(obj) }.getOrNull()
    }

    /** `restic version --json` → `0.19.1`. */
    fun version(output: String): String? =
        runCatching { json.parseToJsonElement(output.trim()).jsonObject.string("version") }.getOrNull()

    // restic 0.17+ nests the message ({"error": {"message": …}}); older
    // releases sent a plain string.
    private fun errorText(e: JsonElement?): String? = when (e) {
        is JsonObject -> e.string("message")
        is JsonPrimitive -> e.contentOrNull
        else -> null
    }

    private fun JsonObject.string(key: String): String? = (this[key] as? JsonPrimitive)?.contentOrNull
}
