package eu.sigmadev.replique

import eu.sigmadev.replique.backup.ForegroundBudget
import eu.sigmadev.replique.data.Interval
import eu.sigmadev.replique.data.KeyGen
import eu.sigmadev.replique.data.FreeUpSpace
import eu.sigmadev.replique.proto.SafeSnapshot
import eu.sigmadev.replique.restic.ResticNode
import java.io.File
import java.nio.file.Files
import java.time.Duration
import java.time.Instant
import java.util.Base64
import java.util.concurrent.TimeUnit
import org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters
import org.bouncycastle.crypto.util.OpenSSHPrivateKeyUtil
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test

class KeyGenTest {
    @Test
    fun ed25519KeyInOpenSshFormat() {
        val keys = KeyGen.ed25519("replique-android@pixel")
        assertTrue(keys.privateKey.startsWith("-----BEGIN OPENSSH PRIVATE KEY-----\n"))
        assertTrue(keys.privateKey.endsWith("-----END OPENSSH PRIVATE KEY-----\n"))
        val parts = keys.publicKey.split(" ")
        assertEquals("ssh-ed25519", parts[0])
        assertEquals("replique-android@pixel", parts[2])

        val body = keys.privateKey.lines().filter { it.isNotEmpty() && !it.startsWith("-----") }.joinToString("")
        val parsed = OpenSSHPrivateKeyUtil.parsePrivateKeyBlob(Base64.getDecoder().decode(body)) as Ed25519PrivateKeyParameters
        val publicBlob = Base64.getDecoder().decode(parts[1])
        // The public key in the line is the private key's own.
        assertTrue(publicBlob.copyOfRange(publicBlob.size - 32, publicBlob.size).contentEquals(parsed.generatePublicKey().encoded))
    }

    /** OpenSSH itself (what rclone's Go SSH library is compatible with) reads the key back. */
    @Test
    fun openSshAcceptsTheKey() {
        val sshKeygen = listOf("/usr/bin/ssh-keygen", "/run/current-system/sw/bin/ssh-keygen").firstOrNull { File(it).canExecute() }
        assumeTrue("ssh-keygen not available", sshKeygen != null)
        val keys = KeyGen.ed25519("test")
        val file = Files.createTempFile("key", "").toFile().apply {
            writeText(keys.privateKey)
            setReadable(false, false); setReadable(true, true)
        }
        val p = ProcessBuilder(sshKeygen, "-y", "-f", file.path).redirectErrorStream(true).start()
        val out = p.inputStream.bufferedReader().readText().trim()
        assertTrue(p.waitFor(10, TimeUnit.SECONDS))
        assertEquals(out, 0, p.exitValue())
        assertEquals(keys.publicKey.substringBeforeLast(' '), out.split(" ").take(2).joinToString(" "))
    }

    @Test
    fun resticPasswordShape() {
        val pw = KeyGen.resticPassword()
        assertTrue(pw, Regex("^([a-km-np-z2-9]{5}-){5}[a-km-np-z2-9]{5}$").matches(pw))
        assertFalse(pw == KeyGen.resticPassword())
    }
}

class ForegroundBudgetTest {
    private val now = Instant.parse("2026-11-14T12:00:00Z")
    private fun hours(h: Long) = Duration.ofHours(h)

    @Test
    fun nothingUsedLeavesSixHoursMinusTheMargin() {
        assertEquals(hours(6).minus(ForegroundBudget.MARGIN), ForegroundBudget.remaining(emptyList(), now))
    }

    @Test
    fun usageOlderThanADayDoesNotCount() {
        val usage = listOf(
            Interval(now.minus(hours(30)), now.minus(hours(26))),
            Interval(now.minus(hours(25)), now.minus(hours(23))),
        )
        assertEquals(hours(1), ForegroundBudget.used(usage, now))
    }

    @Test
    fun exhaustedBudgetComesBackAsIntervalsAgeOut() {
        val usage = listOf(Interval(now.minus(hours(7)), now.minus(hours(1))))
        assertEquals(Duration.ZERO, ForegroundBudget.remaining(usage, now))
        val at = ForegroundBudget.availableAt(usage, now)
        // 25 minutes (margin + minimum run) must age out: the interval started
        // 7 h ago, so that happens 17 h 25 min from now.
        val expected = now.plus(hours(17)).plus(Duration.ofMinutes(25))
        assertTrue("$at vs $expected", Duration.between(expected, at).abs() <= Duration.ofMinutes(2))
        assertTrue(ForegroundBudget.remaining(usage, at) >= ForegroundBudget.MINIMUM_RUN)
    }

    @Test
    fun recordReplacesTheOpenIntervalOfTheSameRun() {
        val start = now.minus(hours(2))
        var usage = ForegroundBudget.record(emptyList(), Interval(start, now.minus(hours(1))), now)
        usage = ForegroundBudget.record(usage, Interval(start, now), now)
        assertEquals(listOf(Interval(start, now)), usage)
    }
}

class FreeUpSpaceTest {
    @Test
    fun onlyUnchangedFilesAreSafe() {
        val dir = Files.createTempDirectory("free").toFile()
        val file = File(dir, "IMG_0001.jpg").apply { writeBytes(ByteArray(1234)) }
        val mtime = Instant.parse("2026-11-13T10:00:00.123456789Z")
        file.setLastModified(mtime.toEpochMilli())
        val snapshot = SafeSnapshot(
            "a3c1", Instant.parse("2026-11-13T23:10:00Z"),
            Instant.parse("2026-11-14T00:02:41Z"), Instant.parse("2026-11-14T04:30:00Z"), "PC",
        )
        val node = ResticNode("IMG_0001.jpg", "file", file.path, 1234, mtime)
        assertTrue(FreeUpSpace.isUnchanged(node, file, snapshot))
        assertFalse("size differs", FreeUpSpace.isUnchanged(node.copy(size = 99), file, snapshot))
        assertFalse("not a file", FreeUpSpace.isUnchanged(node.copy(type = "dir"), file, snapshot))
        file.setLastModified(Instant.parse("2026-11-14T08:00:00Z").toEpochMilli())
        assertFalse("edited since", FreeUpSpace.isUnchanged(node, file, snapshot))
        file.delete()
        assertFalse("gone", FreeUpSpace.isUnchanged(node, file, snapshot))
        assertEquals("DCIM/Camera", FreeUpSpace.group("/storage/emulated/0/DCIM/Camera/IMG_1.jpg"))
        assertEquals("Download", FreeUpSpace.group("/storage/emulated/0/Download/a.pdf"))
    }
}
