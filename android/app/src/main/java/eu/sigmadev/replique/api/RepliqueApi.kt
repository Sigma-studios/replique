package eu.sigmadev.replique.api

import eu.sigmadev.replique.proto.Ack
import eu.sigmadev.replique.proto.Api
import eu.sigmadev.replique.proto.ApiError
import eu.sigmadev.replique.proto.DeviceConfig
import eu.sigmadev.replique.proto.FinishRun
import eu.sigmadev.replique.proto.InstantSerializer
import eu.sigmadev.replique.proto.JobSpec
import eu.sigmadev.replique.proto.PairRequest
import eu.sigmadev.replique.proto.PairResponse
import eu.sigmadev.replique.proto.Progress
import eu.sigmadev.replique.proto.SafeToDelete
import eu.sigmadev.replique.proto.StartRun
import eu.sigmadev.replique.proto.StartRunReply
import eu.sigmadev.replique.proto.Wire
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.net.URLEncoder
import java.time.Instant
import kotlinx.serialization.KSerializer

/** The outcome of one call, sorted the way the protocol asks clients to react. */
sealed interface ApiResult<out T> {
    data class Ok<T>(val value: T, val etag: String? = null) : ApiResult<T>

    /** `304`: the cached copy is current (`GET /device/config` with `If-None-Match`). */
    data object NotModified : ApiResult<Nothing>

    /** A 4xx: a bug or a revoked token. Report locally; retrying will not help. */
    data class Rejected(val status: Int, val error: ApiError?) : ApiResult<Nothing>

    /** Network error or 5xx: the server is unreachable for now. Spool and retry later. */
    data class Unreachable(val reason: String) : ApiResult<Nothing>
}

/** The calls a device makes. An interface so tests can stand in for the server. */
interface RepliqueApi {
    fun pair(request: PairRequest): ApiResult<PairResponse>
    fun deviceConfig(etag: String?): ApiResult<DeviceConfig>
    fun putJob(name: String, spec: JobSpec): ApiResult<Ack>
    fun startRun(start: StartRun): ApiResult<StartRunReply>
    fun progress(runId: String, progress: Progress): ApiResult<Ack>
    fun finish(runId: String, finish: FinishRun): ApiResult<Ack>
    fun safeToDelete(deviceId: String, since: Instant?): ApiResult<SafeToDelete>
}

class HttpRepliqueApi(
    server: String,
    private val token: String?,
    private val userAgent: String,
) : RepliqueApi {
    private val base = server.trimEnd('/')
    private val json = Wire.json

    override fun pair(request: PairRequest) =
        call("POST", Api.PAIR, json.encodeToString(PairRequest.serializer(), request), PairResponse.serializer())

    override fun deviceConfig(etag: String?) =
        call("GET", Api.DEVICE_CONFIG, null, DeviceConfig.serializer(), ifNoneMatch = etag)

    override fun putJob(name: String, spec: JobSpec) =
        call("PUT", Api.job(name), json.encodeToString(JobSpec.serializer(), spec), Ack.serializer())

    override fun startRun(start: StartRun) =
        call("POST", Api.RUNS, json.encodeToString(StartRun.serializer(), start), StartRunReply.serializer())

    override fun progress(runId: String, progress: Progress) =
        call("POST", Api.runProgress(runId), json.encodeToString(Progress.serializer(), progress), Ack.serializer())

    override fun finish(runId: String, finish: FinishRun) =
        call("POST", Api.runFinish(runId), json.encodeToString(FinishRun.serializer(), finish), Ack.serializer())

    override fun safeToDelete(deviceId: String, since: Instant?): ApiResult<SafeToDelete> {
        val query = since?.let { "?since=" + URLEncoder.encode(InstantSerializer.format(it), "UTF-8") } ?: ""
        return call("GET", Api.safeToDelete(deviceId) + query, null, SafeToDelete.serializer())
    }

    private fun <T> call(
        method: String,
        path: String,
        body: String?,
        reply: KSerializer<T>,
        ifNoneMatch: String? = null,
    ): ApiResult<T> {
        val connection = try {
            URL(base + path).openConnection() as HttpURLConnection
        } catch (e: IOException) {
            return ApiResult.Unreachable(e.message ?: e.javaClass.simpleName)
        }
        return try {
            connection.requestMethod = method
            connection.connectTimeout = 15_000
            connection.readTimeout = 60_000
            connection.setRequestProperty("User-Agent", userAgent)
            connection.setRequestProperty("Accept", "application/json")
            token?.let { connection.setRequestProperty("Authorization", "Bearer $it") }
            ifNoneMatch?.let { connection.setRequestProperty("If-None-Match", it) }
            if (body != null) {
                connection.doOutput = true
                connection.setRequestProperty("Content-Type", "application/json")
                connection.outputStream.use { it.write(body.encodeToByteArray()) }
            }
            val status = connection.responseCode
            when {
                status == 304 -> ApiResult.NotModified
                status in 200..299 -> {
                    val text = connection.inputStream.use { it.readBytes().decodeToString() }
                    ApiResult.Ok(json.decodeFromString(reply, text), connection.getHeaderField("ETag"))
                }
                status in 400..499 -> {
                    val text = connection.errorStream?.use { it.readBytes().decodeToString() }
                    val error = text?.let { runCatching { json.decodeFromString(ApiError.serializer(), it) }.getOrNull() }
                    ApiResult.Rejected(status, error)
                }
                else -> ApiResult.Unreachable("HTTP $status")
            }
        } catch (e: IOException) {
            ApiResult.Unreachable(e.message ?: e.javaClass.simpleName)
        } catch (e: kotlinx.serialization.SerializationException) {
            // A 2xx whose body this version cannot read is a server bug, not
            // an outage; retrying the same request would get the same body.
            ApiResult.Rejected(0, ApiError("bad_reply", e.message ?: "unreadable reply"))
        } finally {
            connection.disconnect()
        }
    }
}
