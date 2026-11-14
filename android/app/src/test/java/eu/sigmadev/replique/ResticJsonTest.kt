package eu.sigmadev.replique

import eu.sigmadev.replique.restic.ResticJson
import eu.sigmadev.replique.restic.ResticMessage
import java.time.Instant
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * restic 0.19.1's real `--json` output (recorded from the pinned release, with
 * the recording machine's paths and names replaced) parsed into what the app
 * reports.
 */
class ResticJsonTest {
    private fun lines(name: String): List<String> =
        javaClass.getResource("/restic/$name")!!.readText().lines().filter { it.isNotBlank() }

    private fun messages(name: String) = lines(name).map { ResticJson.parseLine(it) }

    @Test
    fun statusBecomesProgress() {
        val statuses = messages("backup_status.jsonl").filterIsInstance<ResticMessage.Status>()
        assertTrue(statuses.size >= 3)
        // restic leaves out zero values (omitempty): the first status has no
        // seconds_elapsed and no files_done yet.
        val first = statuses.first().status
        assertEquals(0L, first.secondsElapsed)
        assertEquals(0L, first.filesDone)
        assertEquals(1_200_000_000L, first.totalBytes)
        val at = Instant.parse("2026-11-14T05:40:12Z")
        val last = statuses.last().status.toProgress(at)
        assertEquals(at, last.at)
        assertEquals(0.990554695, last.percentDone, 1e-9)
        assertEquals(4L, last.filesDone)
        assertEquals(6L, last.totalFiles)
        assertEquals(1_188_665_634L, last.bytesDone)
        assertEquals(listOf("/storage/emulated/0/Movies/file6.bin"), last.currentFiles)
        val summary = messages("backup_status.jsonl").filterIsInstance<ResticMessage.Summary>().single().summary
        assertEquals(6L, summary.filesNew)
    }

    @Test
    fun successfulBackupSummary() {
        val summary = messages("backup_success.jsonl").filterIsInstance<ResticMessage.Summary>().single().summary
        assertEquals(3L, summary.filesNew)
        assertEquals(2L, summary.filesUnmodified)
        assertEquals(3042L, summary.dataAddedPacked)
        assertEquals(5L, summary.totalFilesProcessed)
        assertEquals("7b6b5f7dda0b94a69329f94c13a20ed7690beae47e6c94913d2137b57c0d7f2f", summary.snapshotId)
        // restic writes local time with an offset and microseconds.
        assertEquals(Instant.parse("2026-11-14T21:57:35.295262Z"), summary.backupStart)
    }

    @Test
    fun partialBackupErrors() {
        val all = messages("backup_partial.jsonl")
        val error = all.filterIsInstance<ResticMessage.Error>().single().error
        assertEquals("archival", error.during)
        assertEquals("/storage/emulated/0/locked/secret.txt", error.item)
        assertTrue(error.message.contains("permission denied"))
        val exit = all.filterIsInstance<ResticMessage.ExitError>().single()
        assertEquals(3, exit.code)
        assertTrue(all.any { it is ResticMessage.Summary })
    }

    @Test
    fun fatalErrors() {
        val wrong = messages("backup_wrong_password.jsonl").single() as ResticMessage.ExitError
        assertEquals(12, wrong.code)
        assertEquals("Fatal: wrong password or no key found", wrong.message)
        val missing = messages("backup_repository_missing.jsonl").single() as ResticMessage.ExitError
        assertEquals(10, missing.code)
        assertTrue(missing.message.startsWith("Fatal: repository does not exist"))
    }

    @Test
    fun checkSummary() {
        val check = messages("check.jsonl").single() as ResticMessage.CheckSummary
        assertEquals(0L, check.numErrors)
        assertTrue(check.brokenPacks.isEmpty())
    }

    @Test
    fun snapshotsVersionInitAndLs() {
        val snapshots = ResticJson.snapshots(lines("snapshots.json").single())
        assertEquals(2, snapshots.size)
        assertEquals("05d273ddd717f70c0498636b39d61e42c0120ff0adb189932878616acd1fbbf6", snapshots[0].id)
        assertEquals(listOf("/storage/emulated/0"), snapshots[1].paths)

        assertEquals("0.19.1", ResticJson.version(lines("version.json").single()))
        assertEquals(ResticMessage.Other("initialized"), ResticJson.parseLine(lines("init.jsonl").single()))

        val ls = lines("ls.jsonl")
        assertNull("the snapshot line is not a node", ResticJson.lsNode(ls.first()))
        val nodes = ls.mapNotNull(ResticJson::lsNode)
        val photo = nodes.single { it.name == "IMG_0001.jpg" }
        assertEquals("file", photo.type)
        assertEquals("/storage/emulated/0/DCIM/Camera/IMG_0001.jpg", photo.path)
        assertEquals(3_000_000L, photo.size)
        assertTrue(photo.mtime != null)
        assertTrue(nodes.any { it.type == "dir" && it.path == "/storage/emulated/0/DCIM" })
    }

    @Test
    fun notJson() {
        assertNull(ResticJson.parseLine("Fatal: something"))
        assertNull(ResticJson.parseLine("[{\"id\":\"x\"}]"))
        assertNull(ResticJson.parseLine("{not json"))
        assertNull(ResticJson.parseLine("{\"version\":2}"))
    }
}
