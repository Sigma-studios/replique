package eu.sigmadev.replique

import android.content.Context
import android.os.ParcelFileDescriptor
import androidx.test.platform.app.InstrumentationRegistry
import eu.sigmadev.replique.proto.StorageInfo
import eu.sigmadev.replique.restic.RepositoryAccess
import eu.sigmadev.replique.restic.Restic
import eu.sigmadev.replique.restic.ResticPaths
import java.io.File
import java.util.Base64
import java.util.UUID
import org.junit.Assume.assumeTrue

/**
 * The local SFTP server started by scripts/test-sftp.sh, as the instrumented
 * tests see it. Its key and host key arrive as instrumentation arguments
 * (`test-sftp.sh args` prints them); without them the tests are skipped.
 */
object TestServer {
    private val args get() = InstrumentationRegistry.getArguments()

    val context: Context get() = InstrumentationRegistry.getInstrumentation().targetContext

    /** The emulator's name for the machine running Docker. */
    val host: String get() = args.getString("sftpHost") ?: "10.0.2.2"
    val port: Int get() = args.getString("sftpPort")?.toInt() ?: 2224

    /** A DNS name for [host], to check that the binaries resolve names (see the DNS test). */
    val hostname: String? get() = args.getString("sftpHostname")?.takeIf { it.isNotBlank() }

    val privateKey: String
        get() {
            val encoded = args.getString("sftpKey")
            assumeTrue("no sftpKey argument: start scripts/test-sftp.sh and pass its args", encoded != null)
            return String(Base64.getDecoder().decode(encoded))
        }

    /** `ssh-ed25519 AAAA…`, the server's host key. */
    private val hostKey: String? get() = args.getString("sftpHostKey")?.let { String(Base64.getDecoder().decode(it)) }

    /** A fresh repository path, so tests never see each other's snapshots. */
    fun storage(hostOverride: String = host) = StorageInfo(
        host = hostOverride,
        port = port,
        username = "replique",
        repoPath = "/home/test-${UUID.randomUUID()}/restic",
        knownHosts = hostKey?.let { listOf("[$hostOverride]:$port $it") } ?: emptyList(),
    )

    fun paths() = ResticPaths(File(context.applicationInfo.nativeLibraryDir), context.filesDir, context.cacheDir)

    fun restic(storage: StorageInfo = storage()) =
        Restic(paths(), RepositoryAccess(storage, privateKey, "test-password-" + UUID.randomUUID()), "emulator")

    /** "All files access" for the app under test, as the user would grant it in Settings. */
    fun grantAllFilesAccess() {
        shell("appops set --uid ${context.packageName} MANAGE_EXTERNAL_STORAGE allow")
    }

    fun shell(command: String): String {
        val pfd = InstrumentationRegistry.getInstrumentation().uiAutomation.executeShellCommand(command)
        return ParcelFileDescriptor.AutoCloseInputStream(pfd).use { it.readBytes().decodeToString() }
    }

    /** A folder of test files in shared storage, where the real backups read. */
    fun dataDir(): File {
        val dir = File("/storage/emulated/0/Download/replique-test-${UUID.randomUUID()}")
        check(dir.mkdirs()) { "cannot create $dir: is all files access granted?" }
        return dir
    }
}
