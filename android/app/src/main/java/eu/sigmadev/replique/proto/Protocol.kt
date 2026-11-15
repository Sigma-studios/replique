// The Kotlin mirror of crates/replique-proto: every message the app and
// Réplique's server exchange. The Rust crate is the reference; names, optional
// fields and defaults follow it field for field, and the unit tests parse every
// file in proto/fixtures/ so the two sides stay on the same bytes.
//
// Compatibility rule, as in the crate: nothing here rejects unknown fields
// (Wire.json ignores them), so the server may add optional fields freely.
@file:OptIn(ExperimentalSerializationApi::class)

package eu.sigmadev.replique.proto

import java.net.URI
import java.net.URLDecoder
import java.net.URLEncoder
import java.time.Instant
import kotlinx.serialization.ExperimentalSerializationApi
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonClassDiscriminator

object Api {
    const val PREFIX = "/api/v1"
    const val PAIR = "/api/v1/pair"
    const val DEVICE_CONFIG = "/api/v1/device/config"
    const val RUNS = "/api/v1/runs"
    const val MIRROR_SOURCES = "/api/v1/mirror-sources"

    fun job(name: String) = "/api/v1/jobs/$name"
    fun runProgress(runId: String) = "/api/v1/runs/$runId/progress"
    fun runFinish(runId: String) = "/api/v1/runs/$runId/finish"
    fun safeToDelete(deviceId: String) = "/api/v1/devices/$deviceId/safe-to-delete"

    const val TOKEN_PREFIX = "rpl_"

    /** Devices send progress at most this often; the server may drop faster ones. */
    const val PROGRESS_MIN_INTERVAL_SECS = 10L

    /** A finish report carries at most this many errors; error_count has the total. */
    const val MAX_REPORTED_ERRORS = 50

    fun isWellFormedToken(token: String): Boolean {
        val rest = token.removePrefix(TOKEN_PREFIX)
        return rest.length != token.length && rest.length == 43 &&
            rest.all { it.isLetterOrDigit() && it.code < 128 || it == '-' || it == '_' }
    }

    fun validJobName(name: String): Boolean =
        name.isNotEmpty() && name.length <= 64 &&
            (name[0] in 'a'..'z' || name[0] in '0'..'9') &&
            name.all { it in 'a'..'z' || it in '0'..'9' || it == '-' || it == '_' }
}

/** The body of every non-2xx reply. */
@Serializable
data class ApiError(val error: String, val message: String)

@Serializable
data class Ack(val ok: Boolean)

// Pairing --------------------------------------------------------------------

@Serializable(with = DeviceKindSerializer::class)
enum class DeviceKind(val wire: String) {
    Nixos("nixos"),
    Android("android"),
    Facade("facade"),

    /** Also what this app reads for a kind added after it was built. */
    Other("other"),
}

object DeviceKindSerializer : LenientEnumSerializer<DeviceKind>(
    "DeviceKind", DeviceKind.entries, DeviceKind::wire, DeviceKind.Other,
)

@Serializable
data class PairRequest(
    val code: String,
    @SerialName("device_name") val deviceName: String,
    val kind: DeviceKind,
    @SerialName("ssh_public_key") val sshPublicKey: String,
    @SerialName("client_version") val clientVersion: String? = null,
)

@Serializable
data class PairResponse(
    @SerialName("device_id") val deviceId: String,
    @SerialName("device_name") val deviceName: String,
    @SerialName("report_token") val reportToken: String,
    val storage: StorageInfo,
)

/** `replique://pair?server=<url>&code=<code>`, the content of the pairing QR code. */
data class PairingLink(val server: String, val code: String) {
    fun toUri(): String =
        "$SCHEME://pair?server=${encode(server)}&code=${encode(code)}"

    companion object {
        const val SCHEME = "replique"

        fun parse(uri: String): PairingLink? {
            val parsed = runCatching { URI(uri.trim()) }.getOrNull() ?: return null
            if (parsed.scheme != SCHEME || parsed.host != "pair") return null
            var server: String? = null
            var code: String? = null
            for (pair in (parsed.rawQuery ?: "").split('&')) {
                val eq = pair.indexOf('=')
                if (eq < 0) continue
                val value = URLDecoder.decode(pair.substring(eq + 1), "UTF-8")
                when (pair.substring(0, eq)) {
                    "server" -> server = value.trimEnd('/')
                    "code" -> code = value
                }
            }
            return PairingLink(
                server = server?.takeIf { it.isNotEmpty() } ?: return null,
                code = code?.takeIf { it.isNotEmpty() } ?: return null,
            )
        }

        // URLEncoder writes spaces as "+", which form decoding (the Rust url
        // crate, URLDecoder) reads back as a space, so the round trip holds.
        private fun encode(s: String) = URLEncoder.encode(s, "UTF-8")
    }
}

object PairingCode {
    const val ALPHABET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789"
    const val LENGTH = 8

    /** Comparison ignores case, spaces and dashes; this is the canonical form. */
    fun normalize(code: String): String =
        code.filter { !it.isWhitespace() && it != '-' }.uppercase()
}

// Storage --------------------------------------------------------------------

object Storage {
    const val STORAGE_BOX_SSH_PORT = 23
    const val DEVICE_REPO_PATH = "/home/restic"

    /**
     * `slugify` in the crate: lowercase ASCII letters and digits (French
     * accents folded), anything else collapsed to single dashes, at most 48
     * characters, `device` when nothing is left.
     */
    fun slugify(name: String): String {
        val out = StringBuilder()
        for (raw in name) {
            val c = foldAccent(raw).lowercaseChar()
            if (c in 'a'..'z' || c in '0'..'9') {
                out.append(c)
            } else if (out.isNotEmpty() && out.last() != '-') {
                out.append('-')
            }
        }
        var s = out.toString().trimEnd('-')
        if (s.length > 48) s = s.substring(0, 48).trimEnd('-')
        return s.ifEmpty { "device" }
    }

    private fun foldAccent(c: Char): Char = when (c) {
        'à', 'â', 'ä', 'á', 'ã', 'å', 'À', 'Â', 'Ä', 'Á' -> 'a'
        'ç', 'Ç' -> 'c'
        'é', 'è', 'ê', 'ë', 'É', 'È', 'Ê', 'Ë' -> 'e'
        'î', 'ï', 'í', 'ì', 'Î', 'Ï' -> 'i'
        'ô', 'ö', 'ó', 'ò', 'õ', 'Ô', 'Ö' -> 'o'
        'ù', 'û', 'ü', 'ú', 'Ù', 'Û', 'Ü' -> 'u'
        'ÿ', 'ý' -> 'y'
        'ñ' -> 'n'
        else -> c
    }
}

@Serializable
data class StorageInfo(
    val host: String,
    val port: Int,
    val username: String,
    @SerialName("repo_path") val repoPath: String,
    @SerialName("known_hosts") val knownHosts: List<String> = emptyList(),
) {
    /** The restic repository through rclone; `remote` must name an rclone SFTP remote for this login. */
    fun resticRcloneRepository(remote: String) = "rclone:$remote:$repoPath"
}

// Jobs -----------------------------------------------------------------------

@Serializable
enum class JobKind {
    @SerialName("backup") Backup,
    @SerialName("mirror") Mirror,
}

@Serializable
data class JobSpec(
    val kind: JobKind,
    val schedule: String? = null,
    @SerialName("expected_interval_secs") val expectedIntervalSecs: Long? = null,
    val rules: JobRules,
)

@Serializable
data class JobRules(
    val paths: List<String> = emptyList(),
    val excludes: List<String> = emptyList(),
    @SerialName("exclude_caches") val excludeCaches: Boolean = false,
    @SerialName("generated_excludes_for") val generatedExcludesFor: List<String> = emptyList(),
    val retention: Retention? = null,
    @SerialName("check_subset") val checkSubset: String? = null,
    @SerialName("mirror_source") val mirrorSource: String? = null,
    @SerialName("mirror_path") val mirrorPath: String? = null,
)

/** How a repository forgets old snapshots. The phone always uses [NeverPrune]. */
@Serializable
@JsonClassDiscriminator("policy")
sealed interface Retention {
    val policyName: String

    @Serializable
    @SerialName("never-prune")
    data object NeverPrune : Retention {
        override val policyName get() = "never-prune"
    }

    @Serializable
    @SerialName("keep-deleted")
    data class KeepDeleted(val keep: KeepRules = KeepRules.defaultVersions()) : Retention {
        override val policyName get() = "keep-deleted"
    }

    @Serializable
    @SerialName("versions")
    data class Versions(val keep: KeepRules) : Retention {
        override val policyName get() = "versions"
    }
}

@Serializable
data class KeepRules(
    val last: KeepCount? = null,
    val hourly: KeepCount? = null,
    val daily: KeepCount? = null,
    val weekly: KeepCount? = null,
    val monthly: KeepCount? = null,
    val yearly: KeepCount? = null,
    val within: String? = null,
) {
    companion object {
        fun defaultVersions() = KeepRules(
            daily = KeepCount.Count(7),
            weekly = KeepCount.Count(4),
            monthly = KeepCount.Count(12),
        )
    }
}

/** A `--keep-*` count: a number, or `"unlimited"` in JSON. */
@Serializable(with = KeepCountSerializer::class)
sealed interface KeepCount {
    data class Count(val n: Long) : KeepCount
    data object Unlimited : KeepCount
}

@Serializable
data class MirrorSources(val sources: List<MirrorSource> = emptyList())

@Serializable
data class MirrorSource(
    @SerialName("device_id") val deviceId: String,
    @SerialName("device_name") val deviceName: String,
    val storage: StorageInfo,
)

// The phone's rules ----------------------------------------------------------

@Serializable
data class DeviceConfig(
    val job: String,
    val folders: List<FolderRule> = emptyList(),
    val excludes: List<String> = emptyList(),
    val conditions: Conditions = Conditions(),
    @SerialName("interval_hours") val intervalHours: Int,
    /** `HH:MM` in the phone's time zone: run once a day then, instead of every [intervalHours]. */
    @SerialName("daily_at") val dailyAt: String? = null,
    val retention: Retention,
) {
    companion object {
        const val ANDROID_SHARED_STORAGE = "/storage/emulated/0"

        /** `ANDROID_DEFAULT_EXCLUDES` in the crate. */
        val ANDROID_DEFAULT_EXCLUDES = listOf(
            "/storage/emulated/0/Android/data",
            "/storage/emulated/0/Android/obb",
            ".thumbnails",
            ".trashed-*",
            ".pending-*",
            ".cache",
            "cache",
            "Cache",
        )

        /**
         * `DeviceConfig::android_default()`: what the app uses before it has
         * reached the server once, and what the server answers until the admin
         * edits the phone's rules.
         */
        fun androidDefault() = DeviceConfig(
            job = "phone",
            folders = listOf(FolderRule(path = ANDROID_SHARED_STORAGE)),
            excludes = ANDROID_DEFAULT_EXCLUDES,
            conditions = Conditions(),
            intervalHours = 24,
            retention = Retention.NeverPrune,
        )
    }
}

@Serializable
data class FolderRule(
    val path: String,
    val excludes: List<String> = emptyList(),
    @SerialName("max_file_size_bytes") val maxFileSizeBytes: Long? = null,
    @SerialName("exclude_extensions") val excludeExtensions: List<String> = emptyList(),
    @SerialName("only_extensions") val onlyExtensions: List<String> = emptyList(),
)

@Serializable
data class Conditions(
    @SerialName("unmetered_only") val unmeteredOnly: Boolean = true,
    @SerialName("charging_only") val chargingOnly: Boolean = true,
    @SerialName("min_battery_percent") val minBatteryPercent: Int? = null,
)

// Runs -----------------------------------------------------------------------

@Serializable
enum class RunKind {
    @SerialName("backup") Backup,
    @SerialName("check") Check,
    @SerialName("prune") Prune,
    @SerialName("mirror") Mirror,
}

@Serializable
data class StartRun(
    @SerialName("client_run_id") val clientRunId: String,
    val job: String,
    val kind: RunKind,
    @SerialName("started_at") @Serializable(with = InstantSerializer::class) val startedAt: Instant,
    @SerialName("restic_version") val resticVersion: String? = null,
    @SerialName("client_version") val clientVersion: String? = null,
)

@Serializable
data class StartRunReply(@SerialName("run_id") val runId: String)

@Serializable
data class Progress(
    @Serializable(with = InstantSerializer::class) val at: Instant,
    @SerialName("percent_done") val percentDone: Double = 0.0,
    @SerialName("seconds_elapsed") val secondsElapsed: Long = 0,
    @SerialName("seconds_remaining") val secondsRemaining: Long? = null,
    @SerialName("total_files") val totalFiles: Long = 0,
    @SerialName("files_done") val filesDone: Long = 0,
    @SerialName("total_bytes") val totalBytes: Long = 0,
    @SerialName("bytes_done") val bytesDone: Long = 0,
    @SerialName("error_count") val errorCount: Long = 0,
    @SerialName("current_files") val currentFiles: List<String> = emptyList(),
    val phase: String? = null,
    /** Bytes sent over the network since the run started (Android's per-app counters). */
    @SerialName("bytes_uploaded") val bytesUploaded: Long? = null,
    /** Upload rate over the last few seconds. */
    @SerialName("upload_bytes_per_sec") val uploadBytesPerSec: Long? = null,
)

@Serializable
enum class RunStatus(val wire: String) {
    @SerialName("running") Running("running"),
    @SerialName("success") Success("success"),
    @SerialName("partial") Partial("partial"),
    @SerialName("repository_missing") RepositoryMissing("repository_missing"),
    @SerialName("lock_failed") LockFailed("lock_failed"),
    @SerialName("wrong_password") WrongPassword("wrong_password"),
    @SerialName("interrupted") Interrupted("interrupted"),
    @SerialName("failed") Failed("failed"),
    @SerialName("lost") Lost("lost");

    companion object {
        fun fromResticExit(code: Int): RunStatus = when (code) {
            0 -> Success
            3 -> Partial
            10 -> RepositoryMissing
            11 -> LockFailed
            12 -> WrongPassword
            130 -> Interrupted
            else -> Failed
        }
    }
}

@Serializable
data class FinishRun(
    val status: RunStatus,
    @SerialName("exit_code") val exitCode: Int,
    @SerialName("finished_at") @Serializable(with = InstantSerializer::class) val finishedAt: Instant,
    val summary: BackupSummary? = null,
    val prune: PruneSummary? = null,
    val check: CheckSummary? = null,
    val mirror: MirrorSummary? = null,
    val errors: List<RunError> = emptyList(),
    @SerialName("error_count") val errorCount: Long = 0,
    @SerialName("unreadable_paths") val unreadablePaths: List<String> = emptyList(),
    val message: String? = null,
)

@Serializable
data class RunError(
    val message: String,
    val during: String? = null,
    val item: String? = null,
)

/** restic's backup `summary` message, field for field. */
@Serializable
data class BackupSummary(
    @SerialName("files_new") val filesNew: Long = 0,
    @SerialName("files_changed") val filesChanged: Long = 0,
    @SerialName("files_unmodified") val filesUnmodified: Long = 0,
    @SerialName("dirs_new") val dirsNew: Long = 0,
    @SerialName("dirs_changed") val dirsChanged: Long = 0,
    @SerialName("dirs_unmodified") val dirsUnmodified: Long = 0,
    @SerialName("data_blobs") val dataBlobs: Long = 0,
    @SerialName("tree_blobs") val treeBlobs: Long = 0,
    @SerialName("data_added") val dataAdded: Long = 0,
    @SerialName("data_added_packed") val dataAddedPacked: Long = 0,
    @SerialName("total_files_processed") val totalFilesProcessed: Long = 0,
    @SerialName("total_bytes_processed") val totalBytesProcessed: Long = 0,
    @SerialName("total_duration") val totalDuration: Double = 0.0,
    @SerialName("backup_start") @Serializable(with = InstantSerializer::class) val backupStart: Instant? = null,
    @SerialName("backup_end") @Serializable(with = InstantSerializer::class) val backupEnd: Instant? = null,
    @SerialName("snapshot_id") val snapshotId: String? = null,
)

@Serializable
data class PruneSummary(
    val policy: String,
    @SerialName("snapshots_before") val snapshotsBefore: Long = 0,
    @SerialName("snapshots_removed") val snapshotsRemoved: Long = 0,
    @SerialName("snapshots_tagged_keep") val snapshotsTaggedKeep: Long = 0,
    @SerialName("bytes_freed") val bytesFreed: Long? = null,
)

@Serializable
data class CheckSummary(
    @SerialName("read_data_subset") val readDataSubset: String? = null,
    @SerialName("num_errors") val numErrors: Long = 0,
    @SerialName("snapshot_ids") val snapshotIds: List<String> = emptyList(),
)

@Serializable
data class MirrorSummary(
    @SerialName("source_device_id") val sourceDeviceId: String,
    @SerialName("files_copied") val filesCopied: Long = 0,
    @SerialName("bytes_copied") val bytesCopied: Long = 0,
    @SerialName("files_removed") val filesRemoved: Long = 0,
    @SerialName("files_verified") val filesVerified: Long = 0,
    @SerialName("hash_mismatches") val hashMismatches: List<String> = emptyList(),
    @SerialName("snapshot_ids") val snapshotIds: List<String> = emptyList(),
    @SerialName("total_bytes") val totalBytes: Long? = null,
)

// Safe to delete -------------------------------------------------------------

@Serializable
data class SafeToDelete(val snapshot: SafeSnapshot? = null)

@Serializable
data class SafeSnapshot(
    @SerialName("snapshot_id") val snapshotId: String,
    @SerialName("snapshot_time") @Serializable(with = InstantSerializer::class) val snapshotTime: Instant,
    @SerialName("verified_at") @Serializable(with = InstantSerializer::class) val verifiedAt: Instant,
    @SerialName("mirrored_at") @Serializable(with = InstantSerializer::class) val mirroredAt: Instant,
    @SerialName("mirrored_by") val mirroredBy: String? = null,
)
