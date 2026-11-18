package eu.sigmadev.replique.data

import eu.sigmadev.replique.proto.DeviceConfig
import eu.sigmadev.replique.proto.InstantSerializer
import eu.sigmadev.replique.proto.RunKind
import eu.sigmadev.replique.proto.RunStatus
import eu.sigmadev.replique.proto.SafeToDelete
import eu.sigmadev.replique.proto.StorageInfo
import eu.sigmadev.replique.proto.Wire
import java.io.File
import java.time.Instant
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

/** What pairing established. Everything secret is in [Secrets] instead. */
@Serializable
data class Pairing(
    /** The server's public URL, without a trailing slash. */
    val server: String,
    @SerialName("device_id") val deviceId: String,
    @SerialName("device_name") val deviceName: String,
    val storage: StorageInfo,
    /**
     * restic's `--host` for every snapshot, fixed at pairing. restic picks the
     * parent snapshot by host and paths, so this must never change, or every
     * file would be read again in full.
     */
    @SerialName("restic_host") val resticHost: String,
    @SerialName("paired_at") @Serializable(with = InstantSerializer::class) val pairedAt: Instant,
    /** Set once `cat config` / `init` succeeded; until then each run tries again. */
    @SerialName("repository_ready") val repositoryReady: Boolean = false,
)

/** A finished run as the Home screen shows it. */
@Serializable
data class RunRecord(
    @SerialName("client_run_id") val clientRunId: String,
    val kind: RunKind,
    @SerialName("started_at") @Serializable(with = InstantSerializer::class) val startedAt: Instant,
    @SerialName("finished_at") @Serializable(with = InstantSerializer::class) val finishedAt: Instant,
    val status: RunStatus,
    val message: String? = null,
    @SerialName("snapshot_id") val snapshotId: String? = null,
    @SerialName("files_processed") val filesProcessed: Long = 0,
    @SerialName("bytes_processed") val bytesProcessed: Long = 0,
    @SerialName("bytes_added") val bytesAdded: Long = 0,
    @SerialName("files_new") val filesNew: Long = 0,
    @SerialName("files_changed") val filesChanged: Long = 0,
    @SerialName("error_count") val errorCount: Long = 0,
    @SerialName("unreadable_paths") val unreadablePaths: List<String> = emptyList(),
)

/** A stretch of time the backup's foreground service ran, for Android 15's daily limit. */
@Serializable
data class Interval(
    @Serializable(with = InstantSerializer::class) val start: Instant,
    @Serializable(with = InstantSerializer::class) val end: Instant,
)

@Serializable
data class PersistedState(
    val pairing: Pairing? = null,
    /** The last rules received from the server, and their ETag. */
    val config: DeviceConfig? = null,
    @SerialName("config_etag") val configEtag: String? = null,
    @SerialName("last_backup") val lastBackup: RunRecord? = null,
    @SerialName("last_check") val lastCheck: RunRecord? = null,
    @SerialName("foreground_usage") val foregroundUsage: List<Interval> = emptyList(),
    @SerialName("safe_to_delete") val safeToDelete: SafeToDelete? = null,
    @SerialName("safe_to_delete_checked_at") @Serializable(with = InstantSerializer::class)
    val safeToDeleteCheckedAt: Instant? = null,
) {
    /** The rules in force: the server's, or the default until it has answered once. */
    val effectiveConfig: DeviceConfig get() = config ?: DeviceConfig.androidDefault()
}

/**
 * The app's state, in one JSON file written atomically, and observable by the
 * screens. The backup worker and the UI run in the same process, so one
 * instance (in [eu.sigmadev.replique.RepliqueApp]) serves both.
 */
class Store(private val file: File) {
    private val flow = MutableStateFlow(read())
    val state: StateFlow<PersistedState> = flow.asStateFlow()
    val current: PersistedState get() = flow.value

    private fun read(): PersistedState =
        runCatching { Wire.json.decodeFromString(PersistedState.serializer(), file.readText()) }
            .getOrDefault(PersistedState())

    @Synchronized
    fun update(transform: (PersistedState) -> PersistedState): PersistedState {
        val next = transform(flow.value)
        if (next != flow.value) {
            val tmp = File(file.parentFile, "${file.name}.tmp")
            tmp.writeText(Wire.json.encodeToString(PersistedState.serializer(), next))
            check(tmp.renameTo(file)) { "could not write $file" }
            flow.value = next
        }
        return next
    }
}
