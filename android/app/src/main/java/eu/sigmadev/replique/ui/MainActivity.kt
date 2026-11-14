package eu.sigmadev.replique.ui

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Home
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Icon
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.lifecycleScope
import eu.sigmadev.replique.RepliqueApp
import eu.sigmadev.replique.proto.PairingLink
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch

class MainActivity : ComponentActivity() {
    private val app get() = application as RepliqueApp

    /** A pairing link opened from outside (the camera app scanning the QR code). */
    private var incomingLink by mutableStateOf<PairingLink?>(null)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        incomingLink = linkFrom(intent)
        setContent {
            RepliqueTheme { Root(app, incomingLink) }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        linkFrom(intent)?.let { incomingLink = it }
    }

    override fun onResume() {
        super.onResume()
        // Android resets the daily dataSync allowance when the user brings the
        // app to the foreground; the app's own account follows.
        app.store.update { it.copy(foregroundUsage = emptyList()) }
        // Reports spooled while the server was unreachable go out now.
        lifecycleScope.launch(Dispatchers.IO) {
            val pairing = app.store.current.pairing ?: return@launch
            val secrets = runCatching { app.secretStore.load() }.getOrNull() ?: return@launch
            app.spool.flush(app.apiFor(pairing, secrets))
        }
    }

    private fun linkFrom(intent: Intent?): PairingLink? =
        intent?.takeIf { it.action == Intent.ACTION_VIEW }?.dataString?.let(PairingLink::parse)
}

private enum class Tab(val label: String) { Home("Accueil"), Free("Libérer"), Settings("Réglages") }

@Composable
private fun Root(app: RepliqueApp, incomingLink: PairingLink?) {
    val state by app.store.state.collectAsStateWithLifecycle()
    if (state.pairing == null) {
        PairScreen(app, incomingLink)
        return
    }
    var tab by rememberSaveable { mutableStateOf(Tab.Home) }
    Scaffold(
        bottomBar = {
            NavigationBar {
                Tab.entries.forEach { t ->
                    NavigationBarItem(
                        selected = tab == t,
                        onClick = { tab = t },
                        icon = {
                            Icon(
                                when (t) {
                                    Tab.Home -> Icons.Filled.Home
                                    Tab.Free -> Icons.Filled.Delete
                                    Tab.Settings -> Icons.Filled.Settings
                                },
                                contentDescription = null,
                            )
                        },
                        label = { Text(t.label) },
                    )
                }
            }
        },
    ) { padding ->
        val modifier = Modifier.padding(padding)
        when (tab) {
            Tab.Home -> HomeScreen(app, modifier)
            Tab.Free -> FreeUpSpaceScreen(app, modifier)
            Tab.Settings -> SettingsScreen(app, modifier)
        }
    }
}
