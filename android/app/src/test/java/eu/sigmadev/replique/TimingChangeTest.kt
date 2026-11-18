package eu.sigmadev.replique

import eu.sigmadev.replique.backup.BackupRunner
import eu.sigmadev.replique.proto.DeviceConfig
import eu.sigmadev.replique.proto.FolderRule
import eu.sigmadev.replique.proto.Retention
import eu.sigmadev.replique.proto.WatchedFiles
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Only timing changes redo the schedule: redoing a daily schedule cancels the
 * periodic work, and a run that fetched new folders or watched files must not
 * cancel itself (as it did the night after "Watch Aegis backups").
 */
class TimingChangeTest {
    private val base = DeviceConfig(
        job = "phone",
        folders = listOf(FolderRule(path = "/storage/emulated/0")),
        intervalHours = 24,
        dailyAt = "04:00",
        retention = Retention.NeverPrune,
    )

    @Test
    fun otherRulesLeaveTheScheduleAlone() {
        assertFalse(BackupRunner.timingChanged(base, base.copy(watched = listOf(WatchedFiles.aegis()))))
        assertFalse(BackupRunner.timingChanged(base, base.copy(folders = emptyList(), excludes = listOf("cache"))))
    }

    @Test
    fun timingRedoesIt() {
        assertTrue(BackupRunner.timingChanged(base, base.copy(dailyAt = "02:00")))
        assertTrue(BackupRunner.timingChanged(base, base.copy(dailyAt = null)))
        assertTrue(BackupRunner.timingChanged(base, base.copy(intervalHours = 12)))
        assertTrue(BackupRunner.timingChanged(base, base.copy(conditions = base.conditions.copy(unmeteredOnly = false))))
    }
}
