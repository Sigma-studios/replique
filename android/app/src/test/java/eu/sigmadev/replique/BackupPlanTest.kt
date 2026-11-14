package eu.sigmadev.replique

import eu.sigmadev.replique.backup.BackupRunner
import eu.sigmadev.replique.proto.DeviceConfig
import eu.sigmadev.replique.proto.FolderRule
import eu.sigmadev.replique.proto.Retention
import eu.sigmadev.replique.proto.Wire
import eu.sigmadev.replique.restic.BackupPlan
import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class BackupPlanTest {
    private val fixtures = File(System.getProperty("replique.fixtures") ?: "../../proto/fixtures")

    @Test
    fun dollarSignsNeverReachRestic() {
        assertEquals("?RECYCLE.BIN", BackupPlan.escapePattern("\$RECYCLE.BIN"))
        assertEquals("/a/?b/*", BackupPlan.escapePattern("/a/\$b/*"))
        // Literal paths: glob characters escaped first, then $ becomes ?. A
        // lone "]" means nothing to restic's matcher and stays as it is.
        assertEquals("/x/\\[1] \\*\\?/?HOME\\\\y", BackupPlan.escapeLiteral("/x/[1] *?/\$HOME\\y"))
    }

    @Test
    fun defaultConfig() {
        val plan = BackupPlan.from(DeviceConfig.androidDefault(), exists = { true }, walk = { error("no walk needed") })
        assertEquals(listOf("/storage/emulated/0"), plan.paths)
        assertEquals(DeviceConfig.ANDROID_DEFAULT_EXCLUDES, plan.excludes)
        assertTrue(plan.iexcludes.isEmpty())
    }

    @Test
    fun fixtureConfigFolderRules() {
        val config = Wire.json.decodeFromString(DeviceConfig.serializer(), File(fixtures, "device_config.json").readText())
        val tree = mapOf(
            "/storage/emulated/0/Download" to listOf(
                "/storage/emulated/0/Download/movie.mkv" to 3_000_000_000L,
                "/storage/emulated/0/Download/paper.pdf" to 100_000L,
            ),
            "/storage/emulated/0/Documents" to listOf(
                "/storage/emulated/0/Documents/cv.PDF" to 50_000L,
                "/storage/emulated/0/Documents/song [live].mp3" to 5_000_000L,
                "/storage/emulated/0/Documents/notes" to 10L,
            ),
        )
        val plan = BackupPlan.from(config, exists = { true }, walk = { tree.getValue(it).asSequence() })
        // Download and Documents are inside shared storage: one backup path.
        assertEquals(listOf("/storage/emulated/0"), plan.paths)
        assertTrue("/storage/emulated/0/Movies/Imported" in plan.excludes)
        assertEquals(
            listOf("/storage/emulated/0/Download/**/*.apk", "/storage/emulated/0/Download/**/*.iso"),
            plan.iexcludes,
        )
        // Too big for Download's limit.
        assertTrue("/storage/emulated/0/Download/movie.mkv" in plan.excludes)
        assertFalse("/storage/emulated/0/Download/paper.pdf" in plan.excludes)
        // Documents keeps only pdf, odt, txt (any case); the rest is listed,
        // with its glob characters escaped.
        assertFalse("/storage/emulated/0/Documents/cv.PDF" in plan.excludes)
        assertTrue("/storage/emulated/0/Documents/song \\[live].mp3" in plan.excludes)
        assertTrue("/storage/emulated/0/Documents/notes" in plan.excludes)
    }

    @Test
    fun missingFoldersAreLeftOut() {
        val config = DeviceConfig.androidDefault().copy(
            folders = listOf(FolderRule("/storage/emulated/0/DCIM"), FolderRule("/storage/emulated/0/Nope/")),
        )
        val plan = BackupPlan.from(config, exists = { it == "/storage/emulated/0/DCIM" }, walk = { emptySequence() })
        assertEquals(listOf("/storage/emulated/0/DCIM"), plan.paths)
    }

    @Test
    fun relativeFolderExcludesAreAnchoredUnderTheFolder() {
        val config = DeviceConfig.androidDefault().copy(
            folders = listOf(FolderRule("/storage/emulated/0/Music", excludes = listOf("*.tmp", "/storage/emulated/0/Music/Podcasts"))),
            excludes = emptyList(),
        )
        val plan = BackupPlan.from(config, exists = { true }, walk = { emptySequence() })
        assertEquals(listOf("/storage/emulated/0/Music/**/*.tmp", "/storage/emulated/0/Music/Podcasts"), plan.excludes)
    }

    @Test
    fun declaredJobIsNeverPrune() {
        val spec = BackupRunner.jobSpec(DeviceConfig.androidDefault())
        assertEquals(Retention.NeverPrune, spec.rules.retention)
        assertEquals("every 24h", spec.schedule)
        assertEquals(86400L, spec.expectedIntervalSecs)
        assertEquals(listOf("/storage/emulated/0"), spec.rules.paths)
    }
}
