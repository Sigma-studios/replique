package eu.sigmadev.replique.data

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import eu.sigmadev.replique.proto.Wire
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

/** What must never leave the phone except into restic's and rclone's environment. */
@Serializable
data class Secrets(
    @SerialName("report_token") val reportToken: String,
    @SerialName("restic_password") val resticPassword: String,
    @SerialName("ssh_private_key") val sshPrivateKey: String,
    @SerialName("ssh_public_key") val sshPublicKey: String,
)

interface SecretStore {
    fun load(): Secrets?
    fun save(secrets: Secrets)
    fun clear()
}

/**
 * Keeps [Secrets] in an app-private file, encrypted with an AES key that lives
 * in the Android Keystore and cannot be exported. A copy of the app's files
 * (a rooted backup tool, a lost phone's flash dump) is useless without that
 * phone's Keystore. The secrets reach restic and rclone only as environment
 * variables of their processes, never as plain files.
 */
class KeystoreSecretStore(private val file: File) : SecretStore {
    override fun load(): Secrets? {
        if (!file.exists()) return null
        val bytes = file.readBytes()
        if (bytes.size < IV_LENGTH + 1) return null
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, bytes, 0, IV_LENGTH))
        val plain = cipher.doFinal(bytes, IV_LENGTH, bytes.size - IV_LENGTH)
        return Wire.json.decodeFromString(Secrets.serializer(), plain.decodeToString())
    }

    override fun save(secrets: Secrets) {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, key())
        val sealed = cipher.doFinal(Wire.json.encodeToString(Secrets.serializer(), secrets).encodeToByteArray())
        val tmp = File(file.parentFile, "${file.name}.tmp")
        tmp.writeBytes(cipher.iv + sealed)
        check(tmp.renameTo(file)) { "could not write $file" }
    }

    override fun clear() {
        file.delete()
        val store = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        if (store.containsAlias(ALIAS)) store.deleteEntry(ALIAS)
    }

    private fun key(): SecretKey {
        val store = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        (store.getKey(ALIAS, null) as? SecretKey)?.let { return it }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        return generator.generateKey()
    }

    private companion object {
        const val KEYSTORE = "AndroidKeyStore"
        const val ALIAS = "replique-secrets"
        const val TRANSFORMATION = "AES/GCM/NoPadding"
        const val IV_LENGTH = 12
    }
}
