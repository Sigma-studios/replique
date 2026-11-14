package eu.sigmadev.replique.data

import eu.sigmadev.replique.proto.SafeSnapshot
import eu.sigmadev.replique.restic.Restic
import eu.sigmadev.replique.restic.ResticJson
import eu.sigmadev.replique.restic.ResticNode
import java.io.File
import java.time.Instant
import kotlin.math.abs

/** A file on the phone that is safely in the backups. */
data class Deletable(val path: String, val size: Long, val modified: Instant)

/**
 * Works out which files can go: the server names the newest snapshot that is
 * both verified on Hetzner and copied to the PC; a file is safe when that
 * snapshot holds it with the same size and modification time as the file on
 * the phone now, so it has not changed since. Nothing is ever deleted here:
 * the screen lists them and the user chooses.
 */
object FreeUpSpace {
    /** restic keeps nanoseconds, File.lastModified() milliseconds (sometimes seconds, on FAT cards). */
    private const val MTIME_TOLERANCE_MS = 1000L

    fun isUnchanged(node: ResticNode, file: File, snapshot: SafeSnapshot): Boolean {
        if (node.type != "file" || !file.isFile) return false
        if (file.length() != node.size) return false
        val nodeMtime = node.mtime ?: return false
        val local = file.lastModified()
        return abs(local - nodeMtime.toEpochMilli()) <= MTIME_TOLERANCE_MS &&
            local <= snapshot.snapshotTime.toEpochMilli() + MTIME_TOLERANCE_MS
    }

    /** Lists the snapshot with `restic ls --json` and keeps the files still unchanged on the phone. */
    suspend fun scan(restic: Restic, snapshot: SafeSnapshot): Result<List<Deletable>> {
        val found = mutableListOf<Deletable>()
        val result = restic.run(
            listOf("ls", "--json", snapshot.snapshotId),
            onStdoutLine = { line ->
                val node = ResticJson.lsNode(line) ?: return@run
                val file = File(node.path)
                if (isUnchanged(node, file, snapshot)) {
                    found += Deletable(node.path, node.size, Instant.ofEpochMilli(file.lastModified()))
                }
            },
        )
        if (result.exitCode != 0) {
            return Result.failure(IllegalStateException(result.reason() ?: "restic ls exited with ${result.exitCode}"))
        }
        return Result.success(found.sortedByDescending { it.size })
    }

    /** The folder a file is grouped under on the screen: its first two levels below shared storage. */
    fun group(path: String, root: String = "/storage/emulated/0"): String {
        val rel = path.removePrefix("$root/")
        val parts = rel.split('/')
        return when {
            parts.size <= 1 -> "/"
            parts.size == 2 -> parts[0]
            else -> parts[0] + "/" + parts[1]
        }
    }
}
