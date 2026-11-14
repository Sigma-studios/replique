package eu.sigmadev.replique.proto

import java.time.Instant
import java.time.OffsetDateTime
import java.time.format.DateTimeFormatter
import kotlinx.serialization.KSerializer
import kotlinx.serialization.SerializationException
import kotlinx.serialization.descriptors.PrimitiveKind
import kotlinx.serialization.descriptors.PrimitiveSerialDescriptor
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.encoding.Decoder
import kotlinx.serialization.encoding.Encoder
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonDecoder
import kotlinx.serialization.json.JsonEncoder
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.longOrNull

object Wire {
    /**
     * Unknown fields are ignored (the protocol's additive-change rule), absent
     * optional fields are left out rather than sent as null, like the crate's
     * `skip_serializing_if`.
     */
    val json = Json {
        ignoreUnknownKeys = true
        explicitNulls = false
        encodeDefaults = true
    }
}

/**
 * RFC 3339 timestamps. Written in UTC with a `Z`; read with any offset, which
 * matters for restic's own output (local time with nanoseconds, e.g.
 * `2026-11-14T22:57:34.46027+01:00`). `Instant.parse` on older Android only
 * accepts `Z`, hence the offset formatter.
 */
object InstantSerializer : KSerializer<Instant> {
    override val descriptor: SerialDescriptor =
        PrimitiveSerialDescriptor("eu.sigmadev.replique.Instant", PrimitiveKind.STRING)

    override fun serialize(encoder: Encoder, value: Instant) =
        encoder.encodeString(format(value))

    override fun deserialize(decoder: Decoder): Instant = parse(decoder.decodeString())

    fun format(value: Instant): String = DateTimeFormatter.ISO_INSTANT.format(value)

    fun parse(s: String): Instant = try {
        OffsetDateTime.parse(s, DateTimeFormatter.ISO_OFFSET_DATE_TIME).toInstant()
    } catch (e: Exception) {
        throw SerializationException("not an RFC 3339 timestamp: $s", e)
    }
}

object KeepCountSerializer : KSerializer<KeepCount> {
    override val descriptor: SerialDescriptor =
        PrimitiveSerialDescriptor("eu.sigmadev.replique.KeepCount", PrimitiveKind.STRING)

    override fun serialize(encoder: Encoder, value: KeepCount) {
        val element = when (value) {
            is KeepCount.Count -> JsonPrimitive(value.n)
            KeepCount.Unlimited -> JsonPrimitive("unlimited")
        }
        (encoder as JsonEncoder).encodeJsonElement(element)
    }

    override fun deserialize(decoder: Decoder): KeepCount {
        val p = (decoder as JsonDecoder).decodeJsonElement() as? JsonPrimitive
            ?: throw SerializationException("expected a count or \"unlimited\"")
        if (p.isString) {
            if (p.content == "unlimited") return KeepCount.Unlimited
            throw SerializationException("expected a count or \"unlimited\", got \"${p.content}\"")
        }
        return KeepCount.Count(p.longOrNull ?: throw SerializationException("bad count ${p.content}"))
    }
}

/**
 * An enum that reads values it does not know as a fallback instead of failing,
 * like `#[serde(other)]` in the crate.
 */
open class LenientEnumSerializer<T : Enum<T>>(
    name: String,
    private val values: List<T>,
    private val wire: (T) -> String,
    private val fallback: T,
) : KSerializer<T> {
    override val descriptor: SerialDescriptor =
        PrimitiveSerialDescriptor("eu.sigmadev.replique.$name", PrimitiveKind.STRING)

    override fun serialize(encoder: Encoder, value: T) = encoder.encodeString(wire(value))

    override fun deserialize(decoder: Decoder): T {
        val s = decoder.decodeString()
        return values.firstOrNull { wire(it) == s } ?: fallback
    }
}
