package eu.sigmadev.replique

import androidx.test.ext.junit.runners.AndroidJUnit4
import eu.sigmadev.replique.data.KeyGen
import eu.sigmadev.replique.proto.DeviceConfig
import eu.sigmadev.replique.proto.FolderRule
import eu.sigmadev.replique.restic.BackupPlan
import eu.sigmadev.replique.restic.ResticJson
import eu.sigmadev.replique.restic.ResticMessage
import eu.sigmadev.replique.restic.ResticNode
import java.io.File
import java.util.concurrent.TimeUnit
import kotlin.random.Random
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Early check 2, on a real Android: restic and rclone run from the app's
 * native library directory, reach an SFTP server through restic's rclone
 * backend, and back up a folder of shared storage.
 */
@RunWith(AndroidJUnit4::class)
class ResticOnDeviceTest {
    @Before
    fun setUp() {
        TestServer.grantAllFilesAccess()
    }

    @Test
    fun binariesRunFromNativeLibraryDir(): Unit = runBlocking {
        val paths = TestServer.paths()
        assertEquals(TestServer.context.applicationInfo.nativeLibraryDir, paths.binDir.path)
        assertTrue("librestic.so extracted", paths.restic.isFile && paths.restic.canExecute())
        assertTrue("librclone.so extracted", paths.rclone.isFile && paths.rclone.canExecute())
        assertEquals(BuildConfig.RESTIC_VERSION, TestServer.restic().version())

        val rclone = ProcessBuilder(paths.rclone.path, "version").redirectErrorStream(true).start()
        val out = rclone.inputStream.bufferedReader().readText()
        assertTrue(rclone.waitFor(30, TimeUnit.SECONDS))
        assertEquals(out, 0, rclone.exitValue())
        assertTrue(out, out.contains("v${BuildConfig.RCLONE_VERSION}"))
        assertTrue(out, out.contains("os/type: android"))
    }

    /**
     * Go programs built without cgo resolve names with Go's own DNS client,
     * which reads /etc/resolv.conf; Android has none, so it asks [::1]:53 and
     * every lookup fails. The binaries are built with cgo so that lookups go
     * through bionic's resolver. A name under `.invalid` never exists, so the
     * error says who answered: "no such host" from the system resolver, or a
     * refused connection to [::1]:53 from Go's.
     */
    @Test
    fun hostNamesResolveThroughTheSystemResolver() {
        val paths = TestServer.paths()
        val rclone = ProcessBuilder(
            paths.rclone.path, "lsf", "--contimeout", "5s", "--low-level-retries", "1", "--retries", "1", "probe:/",
        )
        rclone.environment().putAll(
            mapOf(
                "RCLONE_CONFIG" to "/dev/null",
                "RCLONE_CONFIG_PROBE_TYPE" to "sftp",
                "RCLONE_CONFIG_PROBE_HOST" to "replique-dns-probe.invalid",
                "RCLONE_CONFIG_PROBE_USER" to "nobody",
                // A key made the way pairing makes it: rclone parses it before
                // dialling, so this also proves Go reads the app's key format.
                "RCLONE_CONFIG_PROBE_KEY_PEM" to KeyGen.ed25519("probe").privateKey.trim().replace("\n", "\\n"),
                "RCLONE_CONFIG_PROBE_KNOWN_HOSTS_FILE" to "none",
                "HOME" to TestServer.context.filesDir.path,
            ),
        )
        val process = rclone.redirectErrorStream(true).start()
        val out = process.inputStream.bufferedReader().readText()
        assertTrue(process.waitFor(60, TimeUnit.SECONDS))
        assertFalse("Go's own resolver was used:\n$out", out.contains("[::1]:53") || out.contains("127.0.0.1:53"))
        assertTrue("expected the system resolver's answer:\n$out", out.contains("no such host"))
    }

    /** The full path by name: needs a DNS name for the host (`sftpHostname`, e.g. 10.0.2.2.nip.io). */
    @Test
    fun backupReachesTheServerByName(): Unit = runBlocking {
        val hostname = TestServer.hostname
        assumeTrue("no sftpHostname argument", hostname != null)
        val restic = TestServer.restic(TestServer.storage(hostOverride = hostname!!))
        assertNull(restic.ensureInitialized())
        val data = TestServer.dataDir()
        File(data, "hello.txt").writeText("bonjour")
        val result = restic.run(listOf("backup", "--json", data.path))
        assertEquals(result.stderrTail.joinToString("\n"), 0, result.exitCode)
        data.deleteRecursively()
    }

    @Test
    fun initBackupCheckThroughRclone(): Unit = runBlocking {
        val restic = TestServer.restic()
        // Idempotent init: cat config exits 10, init creates it; a second
        // round finds it and does nothing.
        assertEquals(10, restic.run(listOf("cat", "config")).exitCode)
        assertNull(restic.ensureInitialized())
        assertEquals(0, restic.run(listOf("cat", "config")).exitCode)
        assertNull(restic.ensureInitialized())

        val data = TestServer.dataDir()
        File(data, "DCIM/Camera").mkdirs()
        File(data, "DCIM/Camera/IMG_0001.jpg").writeBytes(Random.nextBytes(2_000_000))
        File(data, "Documents").mkdirs()
        File(data, "Documents/notes.txt").writeText("des notes")
        File(data, "Download").mkdirs()
        File(data, "Download/big.iso").writeBytes(Random.nextBytes(300_000))
        File(data, "\$RECYCLE.BIN").mkdirs()
        File(data, "\$RECYCLE.BIN/deleted.txt").writeText("x")
        File(data, ".thumbnails").mkdirs()
        File(data, ".thumbnails/t.jpg").writeText("thumb")
        File(data, "Android/media/app/cache").mkdirs()
        File(data, "Android/media/app/cache/c.bin").writeText("cache")

        val config = DeviceConfig.androidDefault().copy(
            folders = listOf(
                FolderRule(path = data.path),
                FolderRule(path = "${data.path}/Download", excludeExtensions = listOf("ISO")),
            ),
            excludes = DeviceConfig.ANDROID_DEFAULT_EXCLUDES + "\$RECYCLE.BIN",
        )
        val plan = BackupPlan.from(config)
        assertEquals(listOf(data.path), plan.paths)
        val excludeFile = File(TestServer.context.cacheDir, "test-exclude.txt").apply { writeText(plan.excludes.joinToString("\n")) }
        val iexcludeFile = File(TestServer.context.cacheDir, "test-iexclude.txt").apply { writeText(plan.iexcludes.joinToString("\n")) }

        val messages = mutableListOf<ResticMessage>()
        val result = restic.run(
            listOf("backup", "--json", "--host", "emulator", "--exclude-file", excludeFile.path, "--iexclude-file", iexcludeFile.path) + plan.paths,
        ) { synchronized(messages) { messages += it } }
        assertEquals(result.stderrTail.joinToString("\n"), 0, result.exitCode)
        val summary = messages.filterIsInstance<ResticMessage.Summary>().single().summary
        assertEquals("only the photo and the notes", 2L, summary.filesNew)
        assertNotNull(summary.snapshotId)
        val snapshotId = summary.snapshotId!!

        val nodes = mutableListOf<ResticNode>()
        val ls = restic.run(listOf("ls", "--json", snapshotId), onStdoutLine = { line -> ResticJson.lsNode(line)?.let { nodes += it } })
        assertEquals(0, ls.exitCode)
        val files = nodes.filter { it.type == "file" }.map { it.path.removePrefix(data.path + "/") }.toSet()
        assertEquals(setOf("DCIM/Camera/IMG_0001.jpg", "Documents/notes.txt"), files)

        val (listing, snapshots) = restic.snapshots()
        assertEquals(0, listing.exitCode)
        assertEquals(listOf(snapshotId), snapshots.map { it.id })

        var check: ResticMessage.CheckSummary? = null
        val checked = restic.run(listOf("check", "--json")) { if (it is ResticMessage.CheckSummary) check = it }
        assertEquals(checked.stderrTail.joinToString("\n"), 0, checked.exitCode)
        assertEquals(0L, check!!.numErrors)
        data.deleteRecursively()
    }

    /**
     * Android 15 stops dataSync services after about 6 hours a day; the app
     * then stops restic itself. restic must leave cleanly (no stale lock) and
     * the next run must succeed.
     */
    @Test
    fun stoppedBackupLeavesNoLockAndTheNextRunSucceeds(): Unit = runBlocking {
        val restic = TestServer.restic()
        assertNull(restic.ensureInitialized())
        val data = TestServer.dataDir()
        repeat(8) { File(data, "video$it.mp4").writeBytes(Random.nextBytes(25_000_000)) }

        var sawProgress = false
        val stopped = withTimeoutOrNull(4_000) {
            restic.run(listOf("backup", "--json", data.path)) { if (it is ResticMessage.Status) sawProgress = true }
        }
        assertNull("the backup should still have been running", stopped)

        val locks = restic.run(listOf("list", "locks", "--no-lock"))
        assertEquals(0, locks.exitCode)
        assertEquals("no lock left behind", "", locks.stdout.trim())

        val messages = mutableListOf<ResticMessage>()
        val again = restic.run(listOf("backup", "--json", data.path)) { synchronized(messages) { messages += it } }
        assertEquals(again.stderrTail.joinToString("\n"), 0, again.exitCode)
        val summary = messages.filterIsInstance<ResticMessage.Summary>().single().summary
        android.util.Log.i("ResticOnDeviceTest", "after a stop: progress seen=$sawProgress, resumed run added ${summary.dataAdded} of ${summary.totalBytesProcessed} bytes")
        data.deleteRecursively()
    }
}
