package eu.sigmadev.replique.data

import java.security.SecureRandom
import java.util.Base64
import org.bouncycastle.crypto.generators.Ed25519KeyPairGenerator
import org.bouncycastle.crypto.params.Ed25519KeyGenerationParameters
import org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters
import org.bouncycastle.crypto.util.OpenSSHPrivateKeyUtil
import org.bouncycastle.crypto.util.OpenSSHPublicKeyUtil

/** An SSH key pair in OpenSSH's formats. */
data class SshKeyPair(
    /** `-----BEGIN OPENSSH PRIVATE KEY-----…`, unencrypted (it is encrypted at rest by [SecretStore]). */
    val privateKey: String,
    /** `ssh-ed25519 AAAA… comment`, what the server installs on the sub-account. */
    val publicKey: String,
)

object KeyGen {
    private val random = SecureRandom()

    /**
     * Android's own providers only offer Ed25519 from API 33 and the Keystore
     * cannot export a key for rclone to use, so the key is made with
     * BouncyCastle's lightweight API and written in the openssh-key-v1 format
     * that rclone (golang.org/x/crypto/ssh) reads.
     */
    fun ed25519(comment: String): SshKeyPair {
        val generator = Ed25519KeyPairGenerator()
        generator.init(Ed25519KeyGenerationParameters(random))
        val pair = generator.generateKeyPair()
        val privateBlob = OpenSSHPrivateKeyUtil.encodePrivateKey(pair.private as Ed25519PrivateKeyParameters)
        val publicBlob = OpenSSHPublicKeyUtil.encodePublicKey(pair.public)
        val body = Base64.getEncoder().encodeToString(privateBlob).chunked(70).joinToString("\n")
        return SshKeyPair(
            privateKey = "-----BEGIN OPENSSH PRIVATE KEY-----\n$body\n-----END OPENSSH PRIVATE KEY-----\n",
            publicKey = "ssh-ed25519 ${Base64.getEncoder().encodeToString(publicBlob)} $comment".trim(),
        )
    }

    /** No look-alike characters (0/o, 1/l/i): the password is typed into a password manager by hand at worst. */
    private const val PASSWORD_ALPHABET = "abcdefghjkmnpqrstuvwxyz23456789"

    /**
     * The repository password: six groups of five characters from a 31-letter
     * alphabet, about 148 bits. Only the phone and the user's password manager
     * ever hold it.
     */
    fun resticPassword(): String =
        (1..6).joinToString("-") {
            (1..5).map { PASSWORD_ALPHABET[random.nextInt(PASSWORD_ALPHABET.length)] }.joinToString("")
        }
}
