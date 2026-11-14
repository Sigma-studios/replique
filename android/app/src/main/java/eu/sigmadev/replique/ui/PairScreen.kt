package eu.sigmadev.replique.ui

import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.os.Build
import android.os.PersistableBundle
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import eu.sigmadev.replique.RepliqueApp
import eu.sigmadev.replique.data.PairingFlow
import eu.sigmadev.replique.data.PairingResult
import eu.sigmadev.replique.data.PreparedPairing
import eu.sigmadev.replique.proto.PairingLink
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

private sealed interface PairStep {
    data object Enter : PairStep
    data class Password(val prepared: PreparedPairing) : PairStep
    data class Pairing(val prepared: PreparedPairing) : PairStep
    data class Failed(val prepared: PreparedPairing, val reason: String) : PairStep
}

@Composable
fun PairScreen(app: RepliqueApp, incomingLink: PairingLink?) {
    val flow = remember { PairingFlow(app) }
    var step by remember { mutableStateOf<PairStep>(PairStep.Enter) }
    var linkText by remember { mutableStateOf(incomingLink?.toUri() ?: "") }
    var deviceName by remember { mutableStateOf(Build.MODEL) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()

    LaunchedEffect(incomingLink) {
        if (incomingLink != null && step == PairStep.Enter) linkText = incomingLink.toUri()
    }

    fun startPairing(prepared: PreparedPairing) {
        step = PairStep.Pairing(prepared)
        scope.launch {
            when (val r = flow.pair(prepared)) {
                // The store now holds the pairing: Root switches to Home.
                PairingResult.Paired, is PairingResult.PairedRepositoryPending -> Unit
                is PairingResult.Failed -> step = PairStep.Failed(prepared, r.reason)
            }
        }
    }

    val scanner = rememberLauncherForActivityResult(ScanContract()) { result ->
        result.contents?.let { linkText = it }
    }

    Column(
        Modifier
            .fillMaxSize()
            .safeDrawingPadding()
            .verticalScroll(rememberScrollState())
            .padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Text("Réplique", style = MaterialTheme.typography.headlineLarge, fontWeight = FontWeight.SemiBold)
        when (val s = step) {
            PairStep.Enter -> {
                Text(
                    "Sauvegarde chiffrée du téléphone vers sa propre Storage Box. " +
                        "Ajoutez d'abord ce téléphone sur le site de Réplique, puis scannez le code QR qu'il affiche.",
                )
                Button(
                    onClick = {
                        scanner.launch(
                            ScanOptions()
                                .setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                                .setPrompt("Visez le code QR affiché par Réplique")
                                .setBeepEnabled(false)
                                .setOrientationLocked(true),
                        )
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Scanner le code QR") }
                OutlinedTextField(
                    value = linkText,
                    onValueChange = { linkText = it; error = null },
                    label = { Text("…ou collez le lien replique://pair?…") },
                    modifier = Modifier.fillMaxWidth(),
                    singleLine = true,
                )
                OutlinedTextField(
                    value = deviceName,
                    onValueChange = { deviceName = it },
                    label = { Text("Nom de ce téléphone") },
                    modifier = Modifier.fillMaxWidth(),
                    singleLine = true,
                )
                error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
                Button(
                    onClick = {
                        val link = PairingLink.parse(linkText)
                        if (link == null) {
                            error = "Ce n'est pas un lien d'appairage Réplique."
                        } else {
                            scope.launch {
                                // Ed25519 key generation is quick, but keep it off the UI thread.
                                val prepared = withContext(Dispatchers.Default) { flow.prepare(link, deviceName) }
                                step = PairStep.Password(prepared)
                            }
                        }
                    },
                    enabled = linkText.isNotBlank(),
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Continuer") }
            }

            is PairStep.Password -> PasswordStep(
                prepared = s.prepared,
                onBack = { step = PairStep.Enter },
                onConfirmed = { startPairing(s.prepared) },
            )

            is PairStep.Pairing -> Row(verticalAlignment = Alignment.CenterVertically) {
                CircularProgressIndicator(Modifier.padding(end = 16.dp))
                Text("Appairage avec ${s.prepared.link.server} et préparation du dépôt…")
            }

            is PairStep.Failed -> {
                Text("L'appairage a échoué", style = MaterialTheme.typography.titleMedium)
                Text(s.reason, color = MaterialTheme.colorScheme.error)
                Text(
                    "Le mot de passe affiché reste valable : réessayez avec le même, " +
                        "ou recommencez avec un nouveau code si celui-ci a expiré (15 minutes).",
                )
                Button(onClick = { startPairing(s.prepared) }) { Text("Réessayer") }
                OutlinedButton(onClick = { step = PairStep.Enter }) { Text("Recommencer avec un autre code") }
            }
        }
    }
}

/**
 * The restic password, shown once. Without it nobody can restore this phone's
 * backups (not Réplique, not the PC's copy), so pairing waits until the user
 * says it is saved. The window is marked secure meanwhile so the password does
 * not end up in screenshots or the recent-apps preview.
 */
@Composable
private fun PasswordStep(prepared: PreparedPairing, onBack: () -> Unit, onConfirmed: () -> Unit) {
    val context = LocalContext.current
    var saved by remember { mutableStateOf(false) }
    DisposableEffect(Unit) {
        val window = (context as? ComponentActivity)?.window
        window?.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        onDispose { window?.clearFlags(WindowManager.LayoutParams.FLAG_SECURE) }
    }
    Text("Mot de passe des sauvegardes", style = MaterialTheme.typography.titleLarge)
    Text(
        "Ce mot de passe chiffre les sauvegardes de ce téléphone. Il ne sera plus jamais affiché. " +
            "Enregistrez-le maintenant dans votre gestionnaire de mots de passe : " +
            "sans lui, personne ne pourra restaurer ces sauvegardes, pas même Réplique.",
    )
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
        modifier = Modifier.fillMaxWidth(),
    ) {
        SelectionContainer {
            Text(
                prepared.resticPassword,
                fontFamily = FontFamily.Monospace,
                fontSize = 20.sp,
                modifier = Modifier.padding(20.dp),
            )
        }
    }
    OutlinedButton(onClick = { copySensitive(context, prepared.resticPassword) }) { Text("Copier") }
    Row(verticalAlignment = Alignment.CenterVertically) {
        Checkbox(checked = saved, onCheckedChange = { saved = it })
        Text("J'ai enregistré ce mot de passe dans mon gestionnaire de mots de passe.")
    }
    Spacer(Modifier.height(8.dp))
    Button(onClick = onConfirmed, enabled = saved, modifier = Modifier.fillMaxWidth()) { Text("Appairer ce téléphone") }
    TextButton(onClick = onBack) { Text("Retour") }
}

private fun copySensitive(context: Context, text: String) {
    val clip = ClipData.newPlainText("Réplique", text)
    // Keeps the password out of the clipboard preview Android 13+ shows.
    if (Build.VERSION.SDK_INT >= 24) {
        clip.description.extras = PersistableBundle().apply {
            putBoolean(if (Build.VERSION.SDK_INT >= 33) ClipDescription.EXTRA_IS_SENSITIVE else "android.content.extra.IS_SENSITIVE", true)
        }
    }
    context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clip)
}
