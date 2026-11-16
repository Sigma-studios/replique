package eu.sigmadev.replique

import eu.sigmadev.replique.backup.Watched
import eu.sigmadev.replique.proto.WatchedFiles
import java.time.Instant
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class WatchedTest {
    private val aegis = WatchedFiles.aegis()

    @Test
    fun patternsMatchAsInTheCrate() {
        assertTrue(Watched.matches(aegis.pattern, "aegis-backup-20261116-143000.json"))
        assertTrue(Watched.matches(aegis.pattern, "aegis-backup.json"))
        assertFalse(Watched.matches(aegis.pattern, "aegis-backup-20261116-143000.json.tmp"))
        assertFalse(Watched.matches(aegis.pattern, "aegis-export.json"))
        assertTrue(Watched.matches("a?c*", "abc"))
        assertTrue(Watched.matches("*b*b", "abab"))
        assertFalse(Watched.matches("a?c", "ac"))
    }

    @Test
    fun countsAndNewest() {
        val files = listOf(
            Watched.Entry("aegis-backup-20261101-090000.json", 1_000L),
            Watched.Entry("aegis-backup-20261110-090000.json", 5_000L),
            Watched.Entry("notes.txt", 9_000L),
        )
        val r = Watched.report(listOf(aegis)) { files }.single()
        assertEquals("Aegis backups", r.name)
        assertTrue(r.folderFound)
        assertEquals(2, r.files)
        assertEquals(Instant.ofEpochMilli(5_000L), r.newest)

        val missing = Watched.report(listOf(aegis)) { null }.single()
        assertFalse(missing.folderFound)
        assertEquals(0, missing.files)
        assertNull(missing.newest)
    }
}
