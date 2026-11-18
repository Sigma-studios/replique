package eu.sigmadev.replique

import eu.sigmadev.replique.proto.Ack
import eu.sigmadev.replique.proto.ApiError
import eu.sigmadev.replique.proto.Conditions
import eu.sigmadev.replique.proto.DeviceConfig
import eu.sigmadev.replique.proto.DeviceKind
import eu.sigmadev.replique.proto.FinishRun
import eu.sigmadev.replique.proto.JobKind
import eu.sigmadev.replique.proto.JobSpec
import eu.sigmadev.replique.proto.KeepCount
import eu.sigmadev.replique.proto.MirrorSources
import eu.sigmadev.replique.proto.PairRequest
import eu.sigmadev.replique.proto.PairResponse
import eu.sigmadev.replique.proto.PairingLink
import eu.sigmadev.replique.proto.Progress
import eu.sigmadev.replique.proto.Retention
import eu.sigmadev.replique.proto.RunKind
import eu.sigmadev.replique.proto.RunStatus
import eu.sigmadev.replique.proto.SafeToDelete
import eu.sigmadev.replique.proto.SkippedRun
import eu.sigmadev.replique.proto.StartRun
import eu.sigmadev.replique.proto.StartRunReply
import eu.sigmadev.replique.proto.WatchedFiles
import eu.sigmadev.replique.proto.Wire
import java.io.File
import java.time.Instant
import kotlinx.serialization.KSerializer
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Every file in proto/fixtures/ parses into the Kotlin mirror of the Rust
 * types, survives a round trip, and holds the values the Rust tests expect.
 * A fixture this test does not know fails it, so a new message cannot be added
 * on the Rust side without the app noticing.
 */
class ProtocolFixturesTest {
    private val dir = File(System.getProperty("replique.fixtures") ?: "../../proto/fixtures")
    private val json = Wire.json

    private fun text(name: String) = File(dir, name).readText()

    private fun <T> parse(name: String, serializer: KSerializer<T>): T {
        val value = json.decodeFromString(serializer, text(name))
        // What the app writes reads back the same, and keeps every field the
        // fixture had (absent optional fields stay absent).
        val written = json.encodeToString(serializer, value)
        assertEquals(name, value, json.decodeFromString(serializer, written))
        val original = json.parseToJsonElement(text(name))
        if (original is JsonObject) {
            val rewritten = json.parseToJsonElement(written).jsonObject
            assertTrue("$name lost fields: ${original.keys - rewritten.keys}", rewritten.keys.containsAll(original.keys))
        }
        return value
    }

    private val serializers: Map<String, KSerializer<*>> = mapOf(
        "ack.json" to Ack.serializer(),
        "device_config.json" to DeviceConfig.serializer(),
        "error.json" to ApiError.serializer(),
        "job_spec_backup.json" to JobSpec.serializer(),
        "job_spec_manual.json" to JobSpec.serializer(),
        "job_spec_mirror.json" to JobSpec.serializer(),
        "mirror_sources.json" to MirrorSources.serializer(),
        "pair_request.json" to PairRequest.serializer(),
        "pair_response.json" to PairResponse.serializer(),
        "run_finish_backup.json" to FinishRun.serializer(),
        "run_finish_check.json" to FinishRun.serializer(),
        "run_finish_failed.json" to FinishRun.serializer(),
        "run_finish_mirror.json" to FinishRun.serializer(),
        "run_finish_partial.json" to FinishRun.serializer(),
        "run_finish_prune.json" to FinishRun.serializer(),
        "run_progress.json" to Progress.serializer(),
        "run_progress_phase.json" to Progress.serializer(),
        "run_start.json" to StartRun.serializer(),
        "run_start_reply.json" to StartRunReply.serializer(),
        "skipped_run.json" to SkippedRun.serializer(),
        "safe_to_delete.json" to SafeToDelete.serializer(),
        "safe_to_delete_none.json" to SafeToDelete.serializer(),
    )

    @Test
    fun everyFixtureIsKnownAndParses() {
        val files = dir.listFiles()!!.map { it.name }.filter { it != "README.md" }.sorted()
        assertTrue("no fixtures found in $dir", files.isNotEmpty())
        for (name in files) {
            if (name == "pairing_link.txt") {
                assertTrue(PairingLink.parse(text(name)) != null)
                continue
            }
            val serializer = serializers[name] ?: error("fixture $name has no Kotlin type: mirror the new message")
            parse(name, serializer)
        }
    }

    @Test
    fun pairing() {
        val request = parse("pair_request.json", PairRequest.serializer())
        assertEquals("K7QM-2XPA", request.code)
        assertEquals(DeviceKind.Nixos, request.kind)
        assertTrue(request.sshPublicKey.startsWith("ssh-ed25519 "))

        val response = parse("pair_response.json", PairResponse.serializer())
        assertEquals("dev_01JCX4M8Q2N7R5T3V9W1Y6Z0AB", response.deviceId)
        assertEquals(23, response.storage.port)
        assertEquals("/home/restic", response.storage.repoPath)
        assertEquals("rclone:replique:/home/restic", response.storage.resticRcloneRepository("replique"))
        assertEquals(1, response.storage.knownHosts.size)

        val link = PairingLink.parse(text("pairing_link.txt"))!!
        assertEquals(PairingLink("https://replique.sigma-dev.eu", "K7QM-2XPA"), link)
        assertEquals(text("pairing_link.txt").trim(), link.toUri())

        val error = parse("error.json", ApiError.serializer())
        assertEquals("code_expired", error.error)
    }

    @Test
    fun unknownDeviceKindReadsAsOther() {
        val r = json.decodeFromString(
            PairRequest.serializer(),
            """{"code":"X","device_name":"n","kind":"toaster","ssh_public_key":"k","extra":1}""",
        )
        assertEquals(DeviceKind.Other, r.kind)
    }

    @Test
    fun deviceConfig() {
        val c = parse("device_config.json", DeviceConfig.serializer())
        assertEquals("phone", c.job)
        assertEquals(3, c.folders.size)
        assertEquals(2147483648L, c.folders[1].maxFileSizeBytes)
        assertEquals(WatchedFiles.aegis(), c.watched.single())
        assertEquals(listOf("apk", "iso"), c.folders[1].excludeExtensions)
        assertEquals(listOf("pdf", "odt", "txt"), c.folders[2].onlyExtensions)
        assertEquals(Conditions(unmeteredOnly = true, chargingOnly = true, minBatteryPercent = 30), c.conditions)
        assertEquals(24, c.intervalHours)
        assertEquals(Retention.NeverPrune, c.retention)
        // The fixture's global excludes are exactly the crate's defaults.
        assertEquals(DeviceConfig.ANDROID_DEFAULT_EXCLUDES, c.excludes)
    }

    @Test
    fun deviceConfigDefaultsLikeTheCrate() {
        val c = json.decodeFromString(
            DeviceConfig.serializer(),
            """{"job":"phone","interval_hours":6,"retention":{"policy":"never-prune"},"conditions":{}}""",
        )
        assertEquals(Conditions(unmeteredOnly = true, chargingOnly = true, minBatteryPercent = null), c.conditions)
        assertTrue(c.folders.isEmpty())
        val d = DeviceConfig.androidDefault()
        assertEquals(listOf("/storage/emulated/0"), d.folders.map { it.path })
        assertEquals(24, d.intervalHours)
    }

    @Test
    fun jobsAndRetention() {
        val backup = parse("job_spec_backup.json", JobSpec.serializer())
        assertEquals(JobKind.Backup, backup.kind)
        assertEquals(86400L, backup.expectedIntervalSecs)
        val keep = (backup.rules.retention as Retention.KeepDeleted).keep
        assertEquals(KeepCount.Count(7), keep.daily)
        assertEquals("5%", backup.rules.checkSubset)

        val manual = parse("job_spec_manual.json", JobSpec.serializer())
        assertNull(manual.schedule)
        assertEquals("\$RECYCLE.BIN", manual.rules.excludes[0])
        val versions = manual.rules.retention as Retention.Versions
        assertEquals(KeepCount.Unlimited, versions.keep.monthly)
        assertEquals("14d", versions.keep.within)

        val mirror = parse("job_spec_mirror.json", JobSpec.serializer())
        assertEquals(JobKind.Mirror, mirror.kind)
        assertNull(mirror.rules.retention)
        assertEquals("dev_01JCX5B2K9P4Q8R1S6T3U7V0CD", mirror.rules.mirrorSource)

        // keep-deleted without "keep" gets the default versions, as in Rust.
        val r = json.decodeFromString(Retention.serializer(), """{"policy":"keep-deleted"}""")
        assertEquals(Retention.KeepDeleted(), r)
        assertEquals("""{"policy":"never-prune"}""", json.encodeToString(Retention.serializer(), Retention.NeverPrune))
    }

    @Test
    fun runs() {
        val start = parse("run_start.json", StartRun.serializer())
        assertEquals(RunKind.Backup, start.kind)
        assertEquals(Instant.parse("2026-11-14T03:12:45Z"), start.startedAt)
        assertEquals("0.19.1", start.resticVersion)
        assertEquals("run_01JCX6D4F7G2H5J8K1L3M6N9PQ", parse("run_start_reply.json", StartRunReply.serializer()).runId)

        val progress = parse("run_progress.json", Progress.serializer())
        assertEquals(0.4213, progress.percentDone, 1e-9)
        assertEquals(12153L, progress.secondsRemaining)
        assertEquals(2, progress.currentFiles.size)
        assertNull(progress.phase)
        val phase = parse("run_progress_phase.json", Progress.serializer())
        assertEquals("diffing snapshots", phase.phase)
        assertNull(phase.secondsRemaining)

        val backup = parse("run_finish_backup.json", FinishRun.serializer())
        assertEquals(RunStatus.Success, backup.status)
        assertEquals("60b9632cb99dc43c73727a1da5bb5c48f719c8942bfee76c11b0deadfa744b61", backup.summary!!.snapshotId)
        assertEquals(20928.41, backup.summary!!.totalDuration, 1e-9)
        val aegis = backup.watched.single()
        assertEquals(7, aegis.files)
        assertEquals(java.time.Instant.parse("2026-11-12T18:40:02Z"), aegis.newest)

        val partial = parse("run_finish_partial.json", FinishRun.serializer())
        assertEquals(RunStatus.Partial, partial.status)
        assertEquals("archival", partial.errors.single().during)
        assertEquals(listOf("/home/sigma/.local/share/keyrings"), partial.unreadablePaths)

        val check = parse("run_finish_check.json", FinishRun.serializer())
        assertEquals("5%", check.check!!.readDataSubset)
        assertEquals(2, check.check!!.snapshotIds.size)

        val failed = parse("run_finish_failed.json", FinishRun.serializer())
        assertEquals(RunStatus.Failed, failed.status)
        assertEquals("/mnt/windows/c is not a mountpoint", failed.message)

        val prune = parse("run_finish_prune.json", FinishRun.serializer())
        assertEquals("keep-deleted", prune.prune!!.policy)
        assertEquals(812345678L, prune.prune!!.bytesFreed)

        val mirror = parse("run_finish_mirror.json", FinishRun.serializer())
        assertEquals(38L, mirror.mirror!!.filesVerified)
        assertEquals(84123456789L, mirror.mirror!!.totalBytes)
    }

    @Test
    fun statusFromExitCodes() {
        assertEquals(RunStatus.Success, RunStatus.fromResticExit(0))
        assertEquals(RunStatus.Partial, RunStatus.fromResticExit(3))
        assertEquals(RunStatus.RepositoryMissing, RunStatus.fromResticExit(10))
        assertEquals(RunStatus.LockFailed, RunStatus.fromResticExit(11))
        assertEquals(RunStatus.WrongPassword, RunStatus.fromResticExit(12))
        assertEquals(RunStatus.Interrupted, RunStatus.fromResticExit(130))
        assertEquals(RunStatus.Failed, RunStatus.fromResticExit(1))
        assertEquals(RunStatus.Failed, RunStatus.fromResticExit(-9))
        for (s in RunStatus.entries) {
            assertEquals("\"${s.wire}\"", json.encodeToString(RunStatus.serializer(), s))
        }
    }

    @Test
    fun safeToDelete() {
        val some = parse("safe_to_delete.json", SafeToDelete.serializer())
        assertEquals("PC", some.snapshot!!.mirroredBy)
        assertEquals(Instant.parse("2026-11-13T23:10:00Z"), some.snapshot!!.snapshotTime)
        assertNull(parse("safe_to_delete_none.json", SafeToDelete.serializer()).snapshot)
        assertEquals("{}", json.encodeToString(SafeToDelete.serializer(), SafeToDelete()))
    }

    @Test
    fun mirrorSources() {
        val sources = parse("mirror_sources.json", MirrorSources.serializer())
        assertEquals("Téléphone", sources.sources.single().deviceName)
        assertTrue(sources.sources.single().storage.knownHosts.isEmpty())
    }

    @Test
    fun timestampsWithOffsetsAreAccepted() {
        val p = json.decodeFromString(Progress.serializer(), """{"at":"2026-11-14T06:40:12+01:00"}""")
        assertEquals(Instant.parse("2026-11-14T05:40:12Z"), p.at)
        assertTrue(json.encodeToString(Progress.serializer(), p).contains("\"2026-11-14T05:40:12Z\""))
    }

    @Test
    fun tokensAndJobNames() {
        assertTrue(eu.sigmadev.replique.proto.Api.isWellFormedToken("rpl_q2V0aGlzLWlzLWEtZmFrZS10b2tlbi1mb3ItdGVzdHM"))
        assertFalse(eu.sigmadev.replique.proto.Api.isWellFormedToken("rpl_short"))
        assertTrue(eu.sigmadev.replique.proto.Api.validJobName("phone"))
        assertFalse(eu.sigmadev.replique.proto.Api.validJobName("Phone"))
        assertEquals("telephone-de-sigma", eu.sigmadev.replique.proto.Storage.slugify("Téléphone de Sigma"))
        assertEquals("pc-nixos", eu.sigmadev.replique.proto.Storage.slugify("  PC (NixOS)  "))
        assertEquals("device", eu.sigmadev.replique.proto.Storage.slugify("???"))
        assertEquals("K7QM2XPA", eu.sigmadev.replique.proto.PairingCode.normalize(" k7qm-2xpa "))
    }
}
