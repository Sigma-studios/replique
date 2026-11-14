package eu.sigmadev.replique.restic

import eu.sigmadev.replique.proto.DeviceConfig
import eu.sigmadev.replique.proto.FolderRule
import java.io.File

/**
 * What one `restic backup` covers, worked out from the phone's rules.
 *
 * restic's filters are global, while the rules are per folder, so each folder
 * filter becomes something restic understands: its excludes and extension
 * filters become patterns anchored under the folder, and its size limit and
 * "only these extensions" list (which restic cannot express) become a list of
 * the exact files to leave out, made by walking that folder before the run.
 */
data class BackupPlan(
    /** Top-level folders to back up (a folder inside another is covered by it). */
    val paths: List<String>,
    /** Lines of the `--exclude-file`, already escaped for restic. */
    val excludes: List<String>,
    /** Lines of the `--iexclude-file` (case-insensitive: extensions). */
    val iexcludes: List<String>,
) {
    companion object {
        /**
         * [walk] lists the regular files under a folder as (path, size); the
         * app passes a real directory walk, tests a fake one. It is only called
         * for folders with a size limit or an "only" list.
         */
        fun from(
            config: DeviceConfig,
            exists: (String) -> Boolean = { File(it).exists() },
            walk: (String) -> Sequence<Pair<String, Long>> = ::walkFiles,
        ): BackupPlan {
            val folders = config.folders.map { it.copy(path = it.path.trimEnd('/').ifEmpty { "/" }) }
            val paths = folders.map { it.path }
                .filter { p -> folders.none { other -> other.path != p && isInside(p, other.path) } }
                .distinct()
                .filter(exists)
            val excludes = mutableListOf<String>()
            val iexcludes = mutableListOf<String>()
            config.excludes.mapTo(excludes, ::escapePattern)
            for (folder in folders) {
                for (pattern in folder.excludes) {
                    excludes += anchored(folder, pattern)
                }
                for (ext in folder.excludeExtensions) {
                    iexcludes += escapeLiteral(folder.path) + "/**/*." + escapeLiteral(ext.lowercase().removePrefix("."))
                }
                if (folder.maxFileSizeBytes != null || folder.onlyExtensions.isNotEmpty()) {
                    val only = folder.onlyExtensions.map { it.lowercase().removePrefix(".") }.toSet()
                    if (!exists(folder.path)) continue
                    for ((path, size) in walk(folder.path)) {
                        val tooBig = folder.maxFileSizeBytes?.let { size > it } ?: false
                        val notListed = only.isNotEmpty() && path.substringAfterLast('/').substringAfterLast('.', "").lowercase() !in only
                        if (tooBig || notListed) excludes += escapeLiteral(path)
                    }
                }
            }
            return BackupPlan(paths, excludes.distinct(), iexcludes.distinct())
        }

        /**
         * restic expands `$VARS` in exclude patterns and exclude files
         * (`$RECYCLE.BIN` silently became `.BIN` on the PC), so a literal `$`
         * is written `?`, which matches it and, rarely, one other character.
         */
        fun escapePattern(pattern: String): String = pattern.replace("$", "?")

        /** A path matched literally: glob characters escaped, then `$` as above. */
        fun escapeLiteral(path: String): String = buildString {
            for (c in path) {
                when (c) {
                    '\\', '*', '?', '[' -> append('\\').append(c)
                    '$' -> append('?')
                    else -> append(c)
                }
            }
        }

        // A folder's own pattern applies under that folder only. An absolute
        // pattern is taken as written (the website stores full paths); a
        // relative one is matched at any depth below the folder.
        private fun anchored(folder: FolderRule, pattern: String): String =
            if (pattern.startsWith("/")) escapePattern(pattern)
            else escapeLiteral(folder.path) + "/**/" + escapePattern(pattern)

        private fun isInside(path: String, folder: String): Boolean =
            folder == "/" || path.startsWith("$folder/")

        private fun walkFiles(root: String): Sequence<Pair<String, Long>> =
            File(root).walkTopDown()
                .onFail { _, _ -> }
                .filter { it.isFile }
                .map { it.path to it.length() }
    }
}
