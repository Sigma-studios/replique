package eu.sigmadev.replique.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import eu.sigmadev.replique.BuildConfig
import eu.sigmadev.replique.RepliqueApp
import eu.sigmadev.replique.data.PairingFlow

@Composable
fun SettingsScreen(app: RepliqueApp, modifier: Modifier = Modifier) {
    val state by app.store.state.collectAsStateWithLifecycle()
    val pairing = state.pairing ?: return
    var confirm by remember { mutableStateOf(false) }

    Column(
        modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text("Réglages", style = MaterialTheme.typography.headlineMedium)
        Field("Serveur", pairing.server)
        Field("Ce téléphone", "${pairing.deviceName} (${pairing.deviceId})")
        Field("Appairé", Format.moment(pairing.pairedAt))
        Field(
            "Dépôt",
            "${pairing.storage.username}@${pairing.storage.host}:${pairing.storage.port}${pairing.storage.repoPath}" +
                if (pairing.repositoryReady) "" else " (pas encore créé)",
        )
        Field("Nom d'hôte dans les sauvegardes", pairing.resticHost)
        Field("Mot de passe des sauvegardes", "dans votre gestionnaire de mots de passe ; il n'est jamais réaffiché")
        Field("Version", "Réplique ${BuildConfig.VERSION_NAME} · restic ${BuildConfig.RESTIC_VERSION} · rclone ${BuildConfig.RCLONE_VERSION}")
        Text(
            "Les règles (dossiers, exclusions, conditions) se modifient sur le site de Réplique ; " +
                "le téléphone les reçoit avant chaque sauvegarde.",
            style = MaterialTheme.typography.bodySmall,
        )
        OutlinedButton(onClick = { confirm = true }) { Text("Désappairer ce téléphone") }
    }

    if (confirm) {
        AlertDialog(
            onDismissRequest = { confirm = false },
            title = { Text("Désappairer ce téléphone ?") },
            text = {
                Text(
                    "Le téléphone oublie sa clé, son jeton et le mot de passe des sauvegardes, et ne sauvegarde plus. " +
                        "Les sauvegardes restent sur la Storage Box ; supprimez l'appareil sur le site si vous ne les voulez plus. " +
                        "Pour reprendre plus tard, il faudra un nouveau code d'appairage.",
                )
            },
            confirmButton = { TextButton(onClick = { confirm = false; PairingFlow(app).unpair() }) { Text("Désappairer") } },
            dismissButton = { TextButton(onClick = { confirm = false }) { Text("Annuler") } },
        )
    }
}

@Composable
private fun Field(label: String, value: String) {
    Column {
        Text(label, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.primary)
        Text(value, style = MaterialTheme.typography.bodyMedium)
    }
}
