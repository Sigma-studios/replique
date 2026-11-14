package eu.sigmadev.replique.ui

import android.Manifest
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.work.WorkInfo
import androidx.work.WorkManager
import eu.sigmadev.replique.RepliqueApp
import eu.sigmadev.replique.backup.Scheduler
import eu.sigmadev.replique.data.RunRecord
import eu.sigmadev.replique.proto.DeviceConfig
import eu.sigmadev.replique.proto.RunKind
import eu.sigmadev.replique.proto.RunStatus
import java.time.Instant
import kotlinx.coroutines.flow.map

@Composable
fun HomeScreen(app: RepliqueApp, modifier: Modifier = Modifier) {
    val context = LocalContext.current
    val state by app.store.state.collectAsStateWithLifecycle()
    val live by app.live.collectAsStateWithLifecycle()
    val config = state.effectiveConfig

    // Permissions and battery state can change behind the app's back (in
    // Settings); re-read them whenever the screen comes back.
    var resumed by remember { mutableIntStateOf(0) }
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) { resumed++ }
    val canRead = remember(resumed) { app.canReadSharedStorage() }
    val batteryExempt = remember(resumed) {
        context.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(context.packageName)
    }
    val pending = remember(resumed, state) { app.spool.pending() }

    val nextRun by remember {
        WorkManager.getInstance(context).getWorkInfosForUniqueWorkFlow(Scheduler.SCHEDULED)
            .map { infos -> infos.firstOrNull { it.state == WorkInfo.State.ENQUEUED }?.nextScheduleTimeMillis }
    }.collectAsStateWithLifecycle(initialValue = null)

    val notificationPermission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {}
    val legacyStorage = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { resumed++ }
    LaunchedEffect(Unit) {
        if (Build.VERSION.SDK_INT >= 33) notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
    }

    Column(
        modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("Réplique", style = MaterialTheme.typography.headlineMedium)

        if (!canRead) {
            Notice(
                "Accès aux fichiers requis",
                "Réplique doit pouvoir lire tous les fichiers du téléphone pour les sauvegarder.",
                "Autoriser l'accès à tous les fichiers",
            ) {
                if (Build.VERSION.SDK_INT >= 30) {
                    context.startActivity(
                        Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, Uri.parse("package:${context.packageName}")),
                    )
                } else {
                    legacyStorage.launch(arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE, Manifest.permission.WRITE_EXTERNAL_STORAGE))
                }
            }
        }
        if (!batteryExempt) {
            Notice(
                "Optimisation de la batterie",
                "Sans exemption, Android peut retarder ou refuser les sauvegardes en arrière-plan.",
                "Exempter Réplique",
            ) {
                context.startActivity(
                    Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:${context.packageName}")),
                )
            }
        }

        Section(if (live.running) "En cours" else "Dernière sauvegarde") {
            if (live.running) {
                LiveProgress(live)
                OutlinedButton(onClick = { Scheduler.stopRunning(context, config) }) { Text("Arrêter") }
            } else {
                RunSummary(state.lastBackup, empty = "Aucune sauvegarde pour l'instant.")
                state.lastCheck?.let { check ->
                    Text(
                        "Vérification ${Format.status(check.status)} ${Format.moment(check.finishedAt)}",
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
                Button(
                    onClick = { Scheduler.backupNow(context) },
                    enabled = canRead,
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Sauvegarder maintenant") }
            }
        }

        Section("Prochaine sauvegarde") {
            Text(nextRun?.takeIf { it > 0 && it != Long.MAX_VALUE }?.let { Format.moment(Instant.ofEpochMilli(it)) } ?: "pas encore planifiée")
            Text(conditionsText(config), style = MaterialTheme.typography.bodySmall)
            if (pending > 0) {
                Text(
                    "$pending rapport(s) en attente d'envoi au serveur.",
                    style = MaterialTheme.typography.bodySmall,
                )
            }
        }

        Coverage(config)
    }
}

@Composable
private fun LiveProgress(live: eu.sigmadev.replique.backup.LiveRun) {
    val p = live.progress
    if (live.kind == RunKind.Check) {
        Text(live.phase ?: "Vérification du dépôt…")
        LinearProgressIndicator(Modifier.fillMaxWidth())
        return
    }
    if (p == null) {
        Text(live.phase ?: "Préparation…")
        LinearProgressIndicator(Modifier.fillMaxWidth())
        return
    }
    LinearProgressIndicator(progress = { p.percentDone.toFloat() }, modifier = Modifier.fillMaxWidth())
    Text("${Format.percent(p.percentDone)} · ${Format.bytes(p.bytesDone)} sur ${Format.bytes(p.totalBytes)}")
    Text("${Format.count(p.filesDone)} fichiers sur ${Format.count(p.totalFiles)}", style = MaterialTheme.typography.bodySmall)
    p.secondsRemaining?.let { Text("Encore environ ${Format.duration(it)}", style = MaterialTheme.typography.bodySmall) }
    p.currentFiles.firstOrNull()?.let {
        Text(it.substringAfter("/storage/emulated/0/"), style = MaterialTheme.typography.bodySmall, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
    if (p.errorCount > 0) {
        Text("${p.errorCount} fichier(s) illisible(s)", color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
    }
}

@Composable
private fun RunSummary(run: RunRecord?, empty: String) {
    if (run == null) {
        Text(empty)
        return
    }
    val color = when (run.status) {
        RunStatus.Success -> MaterialTheme.colorScheme.primary
        RunStatus.Partial, RunStatus.Interrupted -> MaterialTheme.colorScheme.secondary
        else -> MaterialTheme.colorScheme.error
    }
    Text("Sauvegarde ${Format.status(run.status)}", color = color, style = MaterialTheme.typography.titleMedium)
    Text(Format.moment(run.finishedAt))
    if (run.filesProcessed > 0) {
        Text(
            "${Format.count(run.filesProcessed)} fichiers, ${Format.bytes(run.bytesProcessed)} · ${Format.bytes(run.bytesAdded)} envoyés",
            style = MaterialTheme.typography.bodySmall,
        )
    }
    run.message?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
    if (run.unreadablePaths.isNotEmpty()) {
        Text(
            "Illisibles : " + run.unreadablePaths.take(3).joinToString { it.substringAfter("/storage/emulated/0/") } +
                if (run.errorCount > 3) "…" else "",
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

private fun conditionsText(config: DeviceConfig): String {
    val parts = mutableListOf("toutes les ${config.intervalHours} h")
    if (config.conditions.unmeteredOnly) parts += "en Wi-Fi"
    if (config.conditions.chargingOnly) parts += "en charge"
    config.conditions.minBatteryPercent?.let { parts += "batterie ≥ $it %" }
    return parts.joinToString(", ").replaceFirstChar { it.uppercase() }
}

/** What is and is not backed up, so the user never has to guess. */
@Composable
private fun Coverage(config: DeviceConfig) {
    var open by remember { mutableStateOf(false) }
    Section("Ce qui est sauvegardé") {
        config.folders.forEach { folder ->
            val extras = buildList {
                folder.maxFileSizeBytes?.let { add("fichiers ≤ ${Format.bytes(it)}") }
                if (folder.onlyExtensions.isNotEmpty()) add("seulement " + folder.onlyExtensions.joinToString())
                if (folder.excludeExtensions.isNotEmpty()) add("sauf " + folder.excludeExtensions.joinToString())
            }
            Text(
                displayPath(folder.path) + if (extras.isNotEmpty()) " (${extras.joinToString("; ")})" else "",
            )
        }
        TextButton(onClick = { open = !open }) { Text(if (open) "Masquer ce qui ne l'est pas" else "Ce qui ne l'est pas") }
        if (open) {
            Text("• Les applications et leurs données privées (réinstallez-les depuis leur source ; leurs propres sauvegardes restent à configurer).")
            (config.excludes + config.folders.flatMap { it.excludes }).distinct().forEach {
                Text("• " + displayPath(it), style = MaterialTheme.typography.bodySmall)
            }
        }
    }
}

private fun displayPath(path: String) = when {
    path == "/storage/emulated/0" -> "Tout le stockage du téléphone"
    path.startsWith("/storage/emulated/0/") -> path.removePrefix("/storage/emulated/0/")
    path.startsWith("/") -> path
    else -> "« $path » (partout)"
}

@Composable
private fun Section(title: String, content: @Composable ColumnScope.() -> Unit) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Text(title, style = MaterialTheme.typography.titleSmall, color = MaterialTheme.colorScheme.primary)
            content()
        }
    }
}

@Composable
private fun Notice(title: String, text: String, action: String, onClick: () -> Unit) {
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.errorContainer),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Text(title, style = MaterialTheme.typography.titleSmall)
            Text(text, style = MaterialTheme.typography.bodyMedium)
            Row { Button(onClick = onClick) { Text(action) } }
        }
    }
}
