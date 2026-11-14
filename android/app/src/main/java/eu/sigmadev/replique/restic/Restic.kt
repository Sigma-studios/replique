package eu.sigmadev.replique.restic

import eu.sigmadev.replique.proto.StorageInfo
import java.io.File
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.runInterruptible
import kotlinx.coroutines.withContext

/** Everything restic needs to open the phone's repository. */
data class RepositoryAccess(
    val storage: StorageInfo,
    /** The OpenSSH private key (`-----BEGIN OPENSSH PRIVATE KEY-----…`). */
    val sshPrivateKey: String,
    val password: String,
)

/** Where the binaries are and where they may write. */
data class ResticPaths(
    /** `applicationInfo.nativeLibraryDir`: the only place an app may execute files from. */
    val binDir: File,
    /** App-private files: HOME, the known_hosts file. */
    val filesDir: File,
    /** App-private cache: restic's cache and temporary pack files. */
    val cacheDir: File,
) {
    val restic get() = File(binDir, "librestic.so")
    val rclone get() = File(binDir, "librclone.so")
}

enum class Stream { Stdout, Stderr }

data class ResticResult(
    val exitCode: Int,
    /** stdout lines that are not JSON messages (e.g. `snapshots --json`'s array), joined. */
    val stdout: String,
    /** The last lines of stderr that were not JSON messages, for error reports. */
    val stderrTail: List<String>,
    /** restic's `exit_error` message, when it sent one. */
    val exitError: String?,
) {
    /** A one-line reason for a failure, from what restic said. */
    fun reason(): String? =
        exitError?.lineSequence()?.firstOrNull()?.removePrefix("Fatal: ")
            ?: stderrTail.lastOrNull { it.isNotBlank() }?.removePrefix("Fatal: ")
}

/**
 * Runs restic against the phone's repository on the Storage Box.
 *
 * Android has no `ssh` binary, so restic's own SFTP backend cannot work here.
 * restic talks to the repository through its rclone backend instead: it starts
 * `librclone.so serve restic --stdio` and speaks HTTP over the pipe, while
 * rclone holds the SFTP connection. The rclone remote is defined entirely by
 * environment variables (RCLONE_CONFIG_REPLIQUE_*), which restic passes down
 * to rclone, so no config file with the key in it is ever written.
 */
class Restic(
    private val paths: ResticPaths,
    private val access: RepositoryAccess,
    /** restic's `--host`: fixed at pairing so the parent snapshot is always found. */
    private val host: String,
) {
    val repository get() = access.storage.resticRcloneRepository(REMOTE)

    fun command(args: List<String>): List<String> = buildList {
        add(paths.restic.absolutePath)
        add("--repo"); add(repository)
        add("--option"); add("rclone.program=${paths.rclone.absolutePath}")
        // restic's default arguments add --b2-hard-delete, a flag the trimmed
        // rclone (SFTP only) does not know and would refuse.
        add("--option"); add("rclone.args=serve restic --stdio")
        add("--cache-dir"); add(File(paths.cacheDir, "restic").absolutePath)
        addAll(args)
    }

    fun environment(): Map<String, String> {
        val tmp = File(paths.cacheDir, "tmp").apply { mkdirs() }
        val env = mutableMapOf(
            "RESTIC_PASSWORD" to access.password,
            // Go defaults to /data/local/tmp on Android, which apps cannot
            // write; restic stages pack files there.
            "TMPDIR" to tmp.absolutePath,
            // Go's os/user on Android only knows $USER and $HOME (it never
            // uses cgo there), and restic records both in its locks.
            "HOME" to paths.filesDir.absolutePath,
            "USER" to "replique",
            // Without a terminal restic prints no progress at all; with this
            // it emits a JSON status message every two seconds.
            "RESTIC_PROGRESS_FPS" to "0.5",
            "RCLONE_CONFIG" to "/dev/null",
            "RCLONE_CONFIG_${REMOTE.uppercase()}_TYPE" to "sftp",
            "RCLONE_CONFIG_${REMOTE.uppercase()}_HOST" to access.storage.host,
            "RCLONE_CONFIG_${REMOTE.uppercase()}_PORT" to access.storage.port.toString(),
            "RCLONE_CONFIG_${REMOTE.uppercase()}_USER" to access.storage.username,
            // rclone reads "\n" escapes in key_pem, which keeps the variable
            // on one line.
            "RCLONE_CONFIG_${REMOTE.uppercase()}_KEY_PEM" to access.sshPrivateKey.trim().replace("\n", "\\n"),
            // Never run commands on the server (md5sum, df…): a Storage Box
            // offers only a few, and restic checks its own data.
            "RCLONE_CONFIG_${REMOTE.uppercase()}_SHELL_TYPE" to "none",
            "RCLONE_CONFIG_${REMOTE.uppercase()}_DISABLE_HASHCHECK" to "true",
        )
        if (access.storage.knownHosts.isNotEmpty()) {
            val knownHosts = File(paths.filesDir, "known_hosts")
            knownHosts.writeText(access.storage.knownHosts.joinToString("\n", postfix = "\n"))
            env["RCLONE_CONFIG_${REMOTE.uppercase()}_KNOWN_HOSTS_FILE"] = knownHosts.absolutePath
            // Go's SSH client asks for whichever host key type it prefers, not
            // the one known_hosts holds; a server with several keys then
            // presents another one and the check fails with "key mismatch".
            // Offering only the known types makes it present the known key.
            env["RCLONE_CONFIG_${REMOTE.uppercase()}_HOST_KEY_ALGORITHMS"] =
                hostKeyAlgorithms(access.storage.knownHosts).joinToString(" ")
        }
        return env
    }

    /**
     * Runs restic with [args]. Each JSON message (stdout or stderr) goes to
     * [onMessage] as it arrives. Cancelling the coroutine stops restic the way
     * Ctrl-C would (SIGTERM: restic removes its lock and exits), waits for it
     * briefly, then rethrows.
     */
    suspend fun run(
        args: List<String>,
        onStdoutLine: ((String) -> Unit)? = null,
        onMessage: (ResticMessage) -> Unit = {},
    ): ResticResult = coroutineScope {
        // restic is started through sh, which prints its own pid and then
        // becomes restic (exec keeps the pid). The pid is needed to stop
        // restic gently: Process.destroy() on Android closes the pipes before
        // signalling, restic's "cleaning up" line then hits a closed stderr,
        // and Go dies of SIGPIPE before removing its lock from the repository.
        // Process.pid() would avoid the shell, but Android's SDK does not
        // offer it at the API levels this app supports.
        val builder = ProcessBuilder(listOf(SHELL, "-c", "echo $PID_MARKER\$\$ >&2; exec \"\$0\" \"\$@\"") + command(args))
        builder.environment().putAll(environment())
        builder.directory(paths.filesDir)
        val process = withContext(Dispatchers.IO) { builder.start() }
        process.outputStream.close()

        val stdout = StringBuilder()
        val stderrTail = ArrayDeque<String>()
        var exitError: String? = null
        val pid = java.util.concurrent.atomic.AtomicInteger(0)
        val lock = Any()

        fun handle(stream: Stream, line: String) {
            if (stream == Stream.Stderr && pid.get() == 0 && line.startsWith(PID_MARKER)) {
                pid.set(line.removePrefix(PID_MARKER).trim().toIntOrNull() ?: 0)
                return
            }
            // Streamed output such as `ls --json` would not fit in memory for a
            // whole phone; it goes to its consumer line by line instead.
            if (stream == Stream.Stdout && onStdoutLine != null) {
                onStdoutLine(line)
                return
            }
            val message = ResticJson.parseLine(line)
            synchronized(lock) {
                when {
                    message is ResticMessage.ExitError -> exitError = message.message
                    message == null && stream == Stream.Stdout -> stdout.appendLine(line)
                    message == null -> {
                        stderrTail.addLast(line)
                        if (stderrTail.size > 40) stderrTail.removeFirst()
                    }
                }
            }
            if (message != null) onMessage(message)
        }

        // The readers keep going while restic cleans up after a stop, so its
        // last words are read rather than met with a closed pipe. If the
        // pipes do get closed under them (the SIGKILL fallback), that is just
        // the end of the output.
        fun drain(input: java.io.InputStream, stream: Stream) = try {
            input.bufferedReader().forEachLine { handle(stream, it) }
        } catch (_: java.io.IOException) {
        }
        val readers = listOf(
            launch(Dispatchers.IO) { drain(process.inputStream, Stream.Stdout) },
            launch(Dispatchers.IO) { drain(process.errorStream, Stream.Stderr) },
        )
        val code = try {
            runInterruptible(Dispatchers.IO) { process.waitFor() }
        } catch (e: CancellationException) {
            withContext(NonCancellable + Dispatchers.IO) { stop(process, pid.get().takeIf { it > 0 }) }
            throw e
        }
        readers.forEach { it.join() }
        synchronized(lock) {
            ResticResult(code, stdout.toString(), stderrTail.toList(), exitError)
        }
    }

    /** SIGTERM, as restic handles it like Ctrl-C (unlock, exit 130); SIGKILL if it hangs. */
    private fun stop(process: Process, pid: Int?) {
        if (pid != null) android.os.Process.sendSignal(pid, SIGTERM) else process.destroy()
        if (!process.waitFor(STOP_GRACE_SECONDS, TimeUnit.SECONDS)) process.destroyForcibly()
    }

    /** `restic version --json`, e.g. `0.19.1`; needs no repository. */
    suspend fun version(): String? {
        var line: String? = null
        run(listOf("version", "--json"), onStdoutLine = { if (line == null) line = it })
        return line?.let(ResticJson::version)
    }

    /**
     * Creates the repository unless it exists: `cat config` exits 10 when
     * there is none, then `init`. Running it again is harmless. Returns the
     * failing result, or null once the repository is there.
     */
    suspend fun ensureInitialized(): ResticResult? {
        val probe = run(listOf("cat", "config"))
        if (probe.exitCode == 0) return null
        if (probe.exitCode != EXIT_REPOSITORY_MISSING) return probe
        val init = run(listOf("init", "--json"))
        return if (init.exitCode == 0) null else init
    }

    suspend fun snapshots(): Pair<ResticResult, List<ResticSnapshot>> {
        val result = run(listOf("snapshots", "--json"))
        val list = if (result.exitCode == 0) runCatching { ResticJson.snapshots(result.stdout) }.getOrNull() else null
        return result to (list ?: emptyList())
    }

    companion object {
        /** The rclone remote's name, as in `rclone:replique:/home/restic`. */
        const val REMOTE = "replique"
        const val EXIT_REPOSITORY_MISSING = 10
        const val EXIT_INTERRUPTED = 130
        private const val STOP_GRACE_SECONDS = 20L
        private const val SIGTERM = 15
        private const val SHELL = "/system/bin/sh"
        private const val PID_MARKER = "replique-pid "

        /**
         * The key types of `known_hosts` lines (`[host]:23 ssh-ed25519 AAAA…`).
         * An RSA key signs with rsa-sha2-*, which servers require nowadays, so
         * those come with it.
         */
        fun hostKeyAlgorithms(knownHosts: List<String>): List<String> =
            knownHosts.mapNotNull { it.trim().split(Regex("\\s+")).getOrNull(1) }
                .flatMap { if (it == "ssh-rsa") listOf("rsa-sha2-512", "rsa-sha2-256", "ssh-rsa") else listOf(it) }
                .distinct()
    }
}
