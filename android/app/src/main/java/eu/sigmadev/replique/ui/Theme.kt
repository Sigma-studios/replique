package eu.sigmadev.replique.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

private val Ink = Color(0xFF1F3A5F)
private val Paper = Color(0xFFFBF8F3)
private val Sand = Color(0xFFF4E9D8)
private val Rust = Color(0xFFB4532A)

private val Light = lightColorScheme(
    primary = Ink,
    onPrimary = Color.White,
    primaryContainer = Color(0xFFD6E2F0),
    onPrimaryContainer = Ink,
    secondary = Rust,
    background = Paper,
    surface = Paper,
    surfaceVariant = Sand,
    error = Color(0xFFB3261E),
)

private val Dark = darkColorScheme(
    primary = Color(0xFFAFC6E4),
    onPrimary = Color(0xFF0E2238),
    primaryContainer = Color(0xFF2E4B70),
    secondary = Color(0xFFE7A27F),
    surfaceVariant = Color(0xFF2B2924),
)

@Composable
fun RepliqueTheme(content: @Composable () -> Unit) {
    MaterialTheme(colorScheme = if (isSystemInDarkTheme()) Dark else Light, content = content)
}
