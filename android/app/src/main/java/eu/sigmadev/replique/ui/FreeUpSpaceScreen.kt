package eu.sigmadev.replique.ui

import android.media.MediaScannerConnection
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import eu.sigmadev.replique.RepliqueApp
import eu.sigmadev.replique.api.ApiResult
import eu.sigmadev.replique.data.Deletable
import eu.sigmadev.replique.data.FreeUpSpace
import java.io.File
import java.time.Instant
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Files that are safely backed up (verified on Hetzner and copied to the PC)
 * and unchanged since. The app only lists them; the user picks and deletes.
 */
@Composable
fun FreeUpSpaceScreen(app: RepliqueApp, modifier: Modifier = Modifier) {
    val context = LocalContext.current
    val state by app.store.state.collectAsStateWithLifecycle()
    val scope = rememberCoroutineScope()
    var busy by remember { mutableStateOf<String?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var files by remember { mutableStateOf<List<Deletable>?>(null) }
    val selected = remember { mutableStateListOf<String>() }
    var expanded by remember { mutableStateOf(setOf<String>()) }
    var confirm by remember { mutableStateOf(false) }
    val snapshot = state.safeToDelete?.snapshot

    fun refresh() {
        scope.launch {
            busy = "Interrogation du serveur…"
            error = null
            val pairing = app.store.current.pairing
            val secrets = withContext(Dispatchers.IO) { app.secretStore.load() }
            if (pairing != null && secrets != null) {
                val reply = withContext(Dispatchers.IO) { app.apiFor(pairing, secrets).safeToDelete(pairing.deviceId, null) }
                when (reply) {
                    is ApiResult.Ok -> app.store.update { it.copy(safeToDelete = reply.value, safeToDeleteCheckedAt = Instant.now()) }
                    is ApiResult.Unreachable -> error = "Serveur injoignable : ${reply.reason}"
                    is ApiResult.Rejected -> error = reply.error?.message ?: "Refusé (HTTP ${reply.status})"
                    ApiResult.NotModified -> Unit
                }
            }
            files = null
            busy = null
        }
    }

    fun scan() {
        val snap = snapshot ?: return
        scope.launch {
            busy = "Liste des fichiers de la sauvegarde…"
            error = null
            val pairing = app.store.current.pairing
            val secrets = withContext(Dispatchers.IO) { app.secretStore.load() }
            if (pairing != null && secrets != null) {
                FreeUpSpace.scan(app.restic(pairing, secrets), snap)
                    .onSuccess { files = it; selected.clear() }
                    .onFailure { error = it.message }
            }
            busy = null
        }
    }

    fun delete() {
        val paths = selected.toList()
        scope.launch {
            busy = "Suppression…"
            val deleted = withContext(Dispatchers.IO) { paths.filter { File(it).delete() } }
            // Tell the media store, or the gallery keeps showing ghosts.
            MediaScannerConnection.scanFile(context, deleted.toTypedArray(), null, null)
            files = files?.filterNot { it.path in deleted }
            selected.clear()
            if (deleted.size < paths.size) error = "${paths.size - deleted.size} fichier(s) n'ont pas pu être supprimés."
            busy = null
        }
    }

    Column(modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
        Text("Libérer de l'espace", style = MaterialTheme.typography.headlineMedium)
        if (snapshot == null) {
            Text(
                "Rien n'est encore sûr à supprimer. Un fichier l'est quand il est dans une sauvegarde " +
                    "vérifiée sur Hetzner et copiée sur le PC.",
            )
            state.safeToDeleteCheckedAt?.let {
                Text("Vérifié ${Format.moment(it)}", style = MaterialTheme.typography.bodySmall)
            }
        } else {
            Text(
                "Sauvegarde du ${Format.moment(snapshot.snapshotTime)}, vérifiée sur Hetzner ${Format.moment(snapshot.verifiedAt)}, " +
                    "copiée sur ${snapshot.mirroredBy ?: "le PC"} ${Format.moment(snapshot.mirroredAt)}.",
                style = MaterialTheme.typography.bodyMedium,
            )
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedButton(onClick = ::refresh, enabled = busy == null) { Text("Actualiser") }
            if (snapshot != null) Button(onClick = ::scan, enabled = busy == null) { Text("Chercher les fichiers") }
        }
        busy?.let {
            Row(verticalAlignment = Alignment.CenterVertically) {
                CircularProgressIndicator(Modifier.padding(end = 12.dp))
                Text(it)
            }
        }
        error?.let { Text(it, color = MaterialTheme.colorScheme.error) }

        val list = files
        if (list != null && busy == null) {
            if (list.isEmpty()) {
                Text("Aucun fichier de cette sauvegarde n'est encore sur le téléphone sans changement.")
            } else {
                val groups = list.groupBy { FreeUpSpace.group(it.path) }.toList().sortedByDescending { (_, f) -> f.sumOf { it.size } }
                val selectedSize = list.filter { it.path in selected }.sumOf { it.size }
                Text("${list.size} fichiers, ${Format.bytes(list.sumOf { it.size })}. Sélection : ${selected.size} (${Format.bytes(selectedSize)}).")
                Button(onClick = { confirm = true }, enabled = selected.isNotEmpty(), modifier = Modifier.fillMaxWidth()) {
                    Text("Supprimer la sélection")
                }
                LazyColumn(Modifier.weight(1f)) {
                    groups.forEach { (group, groupFiles) ->
                        item(key = "g:$group") {
                            val all = groupFiles.all { it.path in selected }
                            Row(
                                Modifier.fillMaxWidth().clickable { expanded = if (group in expanded) expanded - group else expanded + group },
                                verticalAlignment = Alignment.CenterVertically,
                            ) {
                                Checkbox(checked = all, onCheckedChange = { on ->
                                    groupFiles.forEach { f -> if (on) { if (f.path !in selected) selected += f.path } else selected -= f.path }
                                })
                                Column(Modifier.weight(1f)) {
                                    Text(group, style = MaterialTheme.typography.titleSmall)
                                    Text("${groupFiles.size} fichiers, ${Format.bytes(groupFiles.sumOf { it.size })}", style = MaterialTheme.typography.bodySmall)
                                }
                                Text(if (group in expanded) "▲" else "▼", Modifier.padding(8.dp))
                            }
                            HorizontalDivider()
                        }
                        if (group in expanded) {
                            items(groupFiles, key = { it.path }) { f ->
                                Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.padding(start = 24.dp)) {
                                    Checkbox(checked = f.path in selected, onCheckedChange = { on ->
                                        if (on) selected += f.path else selected -= f.path
                                    })
                                    Column {
                                        Text(f.path.substringAfterLast('/'), maxLines = 1, overflow = TextOverflow.Ellipsis)
                                        Text(Format.bytes(f.size), style = MaterialTheme.typography.bodySmall)
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if (confirm) {
        val count = selected.size
        AlertDialog(
            onDismissRequest = { confirm = false },
            title = { Text("Supprimer $count fichier(s) du téléphone ?") },
            text = {
                Text(
                    "Ils restent dans les sauvegardes, sur Hetzner et sur le PC, et peuvent en être restaurés. " +
                        "La suppression sur le téléphone est définitive.",
                )
            },
            confirmButton = { TextButton(onClick = { confirm = false; delete() }) { Text("Supprimer") } },
            dismissButton = { TextButton(onClick = { confirm = false }) { Text("Annuler") } },
        )
    }
}
