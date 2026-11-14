package eu.sigmadev.replique.backup

import eu.sigmadev.replique.proto.Progress
import eu.sigmadev.replique.proto.RunKind
import java.time.Instant
import kotlinx.coroutines.flow.MutableStateFlow

/** What is running right now, for the Home screen and the notification. Never persisted. */
data class LiveRun(
    val running: Boolean = false,
    val kind: RunKind? = null,
    /** A short label while restic has no figures yet ("Préparation du dépôt…"). */
    val phase: String? = null,
    val progress: Progress? = null,
    val startedAt: Instant? = null,
)

typealias LiveRunFlow = MutableStateFlow<LiveRun>
