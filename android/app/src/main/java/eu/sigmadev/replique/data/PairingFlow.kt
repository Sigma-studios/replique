package eu.sigmadev.replique.data

import android.os.Build
import eu.sigmadev.replique.RepliqueApp
import eu.sigmadev.replique.api.ApiResult
import eu.sigmadev.replique.backup.Scheduler
import eu.sigmadev.replique.proto.DeviceKind
import eu.sigmadev.replique.proto.PairRequest
import eu.sigmadev.replique.proto.PairingCode
import eu.sigmadev.replique.proto.PairingLink
import eu.sigmadev.replique.proto.Storage
import java.time.Instant
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/** The key and password made on the phone, before the server has seen them. */
data class PreparedPairing(
    val link: PairingLink,
    val deviceName: String,
    val keys: SshKeyPair,
    val resticPassword: String,
)

sealed interface PairingResult {
    data object Paired : PairingResult

    /** Paired, but creating the repository failed; the first backup tries again. */
    data class PairedRepositoryPending(val reason: String) : PairingResult
    data class Failed(val reason: String) : PairingResult
}

/**
 * Pairing, as the protocol describes it: the phone makes its SSH key and
 * restic password, the user saves the password, `POST /pair` installs the key
 * on the phone's sub-account and returns the report token and where the
 * repository lives, then the repository is created if it is not there yet.
 */
class PairingFlow(private val app: RepliqueApp) {
    fun prepare(link: PairingLink, deviceName: String) = PreparedPairing(
        link = link,
        deviceName = deviceName.trim().ifEmpty { Build.MODEL },
        keys = KeyGen.ed25519("replique-android@${Storage.slugify(deviceName.ifBlank { Build.MODEL })}"),
        resticPassword = KeyGen.resticPassword(),
    )

    suspend fun pair(prepared: PreparedPairing): PairingResult = withContext(Dispatchers.IO) {
        val api = app.apiFor(prepared.link.server, null)
        val request = PairRequest(
            code = PairingCode.normalize(prepared.link.code),
            deviceName = prepared.deviceName,
            kind = DeviceKind.Android,
            sshPublicKey = prepared.keys.publicKey,
            clientVersion = app.userAgent,
        )
        val response = when (val reply = api.pair(request)) {
            is ApiResult.Ok -> reply.value
            is ApiResult.Rejected -> return@withContext PairingResult.Failed(
                reply.error?.message ?: "Le serveur a refusé l'appairage (HTTP ${reply.status}).",
            )
            is ApiResult.Unreachable -> return@withContext PairingResult.Failed(
                "Serveur injoignable : ${reply.reason}",
            )
            ApiResult.NotModified -> return@withContext PairingResult.Failed("Réponse inattendue du serveur.")
        }
        val secrets = Secrets(
            reportToken = response.reportToken,
            resticPassword = prepared.resticPassword,
            sshPrivateKey = prepared.keys.privateKey,
            sshPublicKey = prepared.keys.publicKey,
        )
        app.secretStore.save(secrets)
        val pairing = Pairing(
            server = prepared.link.server,
            deviceId = response.deviceId,
            deviceName = response.deviceName,
            storage = response.storage,
            // The website's name wins, so the host comes from the reply.
            resticHost = Storage.slugify(response.deviceName),
            pairedAt = Instant.now(),
        )
        app.store.update { PersistedState(pairing = pairing) }

        // The rules (and so the schedule) come from the server; until it
        // answers, the defaults apply.
        val runner = app.backupRunner()
        val config = runner.refreshConfig(app.apiFor(pairing, secrets))
        // Without "All files access" a run could only fail; Home schedules it
        // as soon as the user grants it.
        if (app.canReadSharedStorage()) Scheduler.ensureScheduled(app, config)

        val failed = app.restic(pairing, secrets).ensureInitialized()
        if (failed != null) {
            return@withContext PairingResult.PairedRepositoryPending(failed.reason() ?: "restic exit ${failed.exitCode}")
        }
        app.store.update { s -> s.copy(pairing = s.pairing?.copy(repositoryReady = true)) }
        PairingResult.Paired
    }

    /**
     * Forgets the pairing on this phone: secrets, state, schedule. The backups
     * stay on the Storage Box; removing the device is done on the website.
     */
    fun unpair() {
        Scheduler.cancelAll(app)
        app.secretStore.clear()
        app.store.update { PersistedState() }
        app.resticPaths.cacheDir.resolve("restic").deleteRecursively()
        app.filesDir.resolve("known_hosts").delete()
        app.spool.clear()
    }
}
