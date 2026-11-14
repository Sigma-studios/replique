package eu.sigmadev.replique

import android.Manifest
import android.app.Application
import android.content.pm.PackageManager
import android.os.Build
import android.os.Environment
import eu.sigmadev.replique.api.HttpRepliqueApi
import eu.sigmadev.replique.api.RepliqueApi
import eu.sigmadev.replique.api.Spool
import eu.sigmadev.replique.backup.BackupRunner
import eu.sigmadev.replique.backup.LiveRun
import eu.sigmadev.replique.backup.Notifications
import eu.sigmadev.replique.backup.Scheduler
import eu.sigmadev.replique.data.KeystoreSecretStore
import eu.sigmadev.replique.data.Pairing
import eu.sigmadev.replique.data.SecretStore
import eu.sigmadev.replique.data.Secrets
import eu.sigmadev.replique.data.Store
import eu.sigmadev.replique.restic.Restic
import eu.sigmadev.replique.restic.RepositoryAccess
import eu.sigmadev.replique.restic.ResticPaths
import java.io.File
import kotlinx.coroutines.flow.MutableStateFlow

/** The app's single instances: state, secrets, spool, and what is running now. */
class RepliqueApp : Application() {
    lateinit var store: Store
        private set
    lateinit var secretStore: SecretStore
        private set
    lateinit var spool: Spool
        private set
    val live = MutableStateFlow(LiveRun())

    override fun onCreate() {
        super.onCreate()
        store = Store(File(filesDir, "state.json"))
        secretStore = KeystoreSecretStore(File(filesDir, "secrets.bin"))
        spool = Spool(File(filesDir, "spool"))
        Notifications.createChannels(this)
    }

    val userAgent: String
        get() = "replique-android/${BuildConfig.VERSION_NAME} (Android ${Build.VERSION.RELEASE})"

    val resticPaths: ResticPaths
        get() = ResticPaths(File(applicationInfo.nativeLibraryDir), filesDir, cacheDir)

    fun apiFor(server: String, token: String?): RepliqueApi = HttpRepliqueApi(server, token, userAgent)

    fun apiFor(pairing: Pairing, secrets: Secrets): RepliqueApi = apiFor(pairing.server, secrets.reportToken)

    fun restic(pairing: Pairing, secrets: Secrets) = Restic(
        resticPaths,
        RepositoryAccess(pairing.storage, secrets.sshPrivateKey, secrets.resticPassword),
        pairing.resticHost,
    )

    fun backupRunner() = BackupRunner(
        store = store,
        secretStore = secretStore,
        spool = spool,
        paths = resticPaths,
        apiFor = ::apiFor,
        live = live,
        clientVersion = userAgent,
        resticVersion = BuildConfig.RESTIC_VERSION,
        onConfigChanged = { config -> if (canReadSharedStorage()) Scheduler.schedule(this, config) },
    )

    /** "All files access" on Android 11+, the storage permission before. */
    fun canReadSharedStorage(): Boolean =
        if (Build.VERSION.SDK_INT >= 30) {
            Environment.isExternalStorageManager()
        } else {
            checkSelfPermission(Manifest.permission.READ_EXTERNAL_STORAGE) == PackageManager.PERMISSION_GRANTED
        }
}
