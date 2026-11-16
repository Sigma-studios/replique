package eu.sigmadev.replique.backup

import eu.sigmadev.replique.proto.WatchedFiles
import eu.sigmadev.replique.proto.WatchedReport
import java.io.File
import java.time.Instant

/**
 * What the phone reports of its watched files after a backup: per entry, how
 * many files in the folder match and when the newest was written. Names and
 * contents stay on the phone; the server only needs to know whether the app
 * writing them (Aegis) still does.
 */
object Watched {
    /** A file in a watched folder: its name and last-modified time (ms). */
    data class Entry(val name: String, val modifiedMillis: Long)

    fun report(
        watched: List<WatchedFiles>,
        /** The regular files directly in a folder, or null when there is no such folder. */
        list: (String) -> List<Entry>? = ::listFolder,
    ): List<WatchedReport> = watched.map { w ->
        val files = list(w.folder)
        val matching = files.orEmpty().filter { matches(w.pattern, it.name) }
        WatchedReport(
            name = w.name,
            folderFound = files != null,
            files = matching.size,
            newest = matching.maxOfOrNull { it.modifiedMillis }?.let(Instant::ofEpochMilli),
        )
    }

    /** `name_matches` in the crate: `*` any run of characters, `?` one. */
    fun matches(pattern: String, name: String): Boolean {
        var i = 0
        var j = 0
        var starI = -1
        var starJ = 0
        while (j < name.length) {
            if (i < pattern.length && (pattern[i] == '?' || pattern[i] == name[j])) {
                i++; j++
            } else if (i < pattern.length && pattern[i] == '*') {
                starI = i; starJ = j; i++
            } else if (starI >= 0) {
                i = starI + 1; starJ++; j = starJ
            } else {
                return false
            }
        }
        while (i < pattern.length && pattern[i] == '*') i++
        return i == pattern.length
    }

    private fun listFolder(path: String): List<Entry>? {
        val dir = File(path)
        if (!dir.isDirectory) return null
        return dir.listFiles().orEmpty().filter { it.isFile }.map { Entry(it.name, it.lastModified()) }
    }
}
