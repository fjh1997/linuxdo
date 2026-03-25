package io.linuxdo.accelerator.android

import okhttp3.Dns
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONObject
import okio.ByteString.Companion.toByteString
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress
import java.net.URL
import java.util.Base64
import java.util.Locale
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import android.util.Log
import android.os.SystemClock

data class ParsedDnsQuery(
    val id: Int,
    val flags: Int,
    val name: String,
    val type: Int,
    val questionBytes: ByteArray,
)

data class DnsAnswerRecord(
    val type: Int,
    val ttl: Int,
    val data: ByteArray,
)

data class DnsResolution(
    val answers: List<DnsAnswerRecord>,
    val responseCode: Int = 0,
)

data class CachedDnsResolution(
    val resolution: DnsResolution,
    val expiresAtMs: Long,
)

data class DnsCacheKey(
    val host: String,
    val type: Int,
)

data class PreparedDohEndpoint(
    val rawUrl: String,
    val host: String,
    val url: URL,
    val addresses: List<InetAddress>,
)

class LinuxdoDnsResolver(
    private val config: LinuxdoConfig,
    endpoints: List<PreparedDohEndpoint>,
) {
    private val tag = "LinuxdoDnsResolver"
    private val cache = ConcurrentHashMap<DnsCacheKey, CachedDnsResolution>()
    private val client = OkHttpClient.Builder()
        .connectTimeout(10, TimeUnit.SECONDS)
        .readTimeout(10, TimeUnit.SECONDS)
        .dns(StaticDns(endpoints.associate { it.host to it.addresses }))
        .build()

    private val primaryEndpoint = endpoints.firstOrNull()

    fun resolveManagedPayload(requestPayload: ByteArray, query: ParsedDnsQuery): ByteArray {
        val host = query.name.lowercase(Locale.US)
        readCached(host, query.type)?.let { cached ->
            Log.d(tag, "cache hit for $host type=${query.type} answers=${cached.answers.size}")
            return DnsPacketCodec.buildResponse(query, cached)
        }

        resolveLocal(host, query.type)?.let { answers ->
            val resolution = DnsResolution(answers)
            writeCache(host, query.type, resolution)
            Log.d(tag, "resolved local override for $host type=${query.type} answers=${answers.size}")
            return DnsPacketCodec.buildResponse(query, resolution)
        }

        val endpoint = requirePrimaryEndpoint()
        try {
            if (supportsDnsMessage(endpoint.url)) {
                val resolution = queryDohDnsMessageRaw(endpoint, requestPayload, query)
                writeCache(host, query.type, resolution)
                Log.d(tag, "resolved $host type=${query.type} via ${endpoint.rawUrl} raw dns-message")
                return DnsPacketCodec.buildResponse(query, resolution)
            }

            if (isJsonFriendlyType(query.type)) {
                val resolution = DnsResolution(queryDohJson(endpoint, host, query.type))
                if (resolution.answers.isNotEmpty()) {
                    writeCache(host, query.type, resolution)
                    Log.d(tag, "resolved $host type=${query.type} via ${endpoint.rawUrl} answers=${resolution.answers.size}")
                    return DnsPacketCodec.buildResponse(query, resolution)
                }
                throw IOException("empty answer from ${endpoint.rawUrl}")
            }

            throw IOException("endpoint ${endpoint.rawUrl} does not support dns-message for type ${query.type}")
        } catch (error: Exception) {
            Log.w(tag, "resolver endpoint failed for $host via ${endpoint.rawUrl}: ${error.message}")
            throw wrapDohFailure(endpoint, host, query.type, error)
        }
    }

    fun resolve(query: ParsedDnsQuery): DnsResolution {
        if (!isSupportedManagedType(query.type)) {
            return DnsResolution(emptyList())
        }

        val host = query.name.lowercase(Locale.US)
        readCached(host, query.type)?.let {
            Log.d(tag, "cache hit for $host type=${query.type} answers=${it.answers.size}")
            return it
        }

        val managedHost = config.shouldUseManagedDoh(host)
        resolveLocal(host, query.type)?.let {
            val resolution = DnsResolution(it)
            writeCache(host, query.type, resolution)
            return resolution
        }

        if (!managedHost) {
            return DnsResolution(emptyList())
        }

        val endpoint = requirePrimaryEndpoint()
        try {
            val resolution = DnsResolution(queryDoh(endpoint, host, query.type))
            if (resolution.answers.isNotEmpty()) {
                writeCache(host, query.type, resolution)
                Log.d(tag, "resolved $host type=${query.type} via ${endpoint.rawUrl} answers=${resolution.answers.size}")
                return resolution
            }
            throw IOException("empty answer from ${endpoint.rawUrl}")
        } catch (error: Exception) {
            Log.w(tag, "resolver endpoint failed for $host via ${endpoint.rawUrl}: ${error.message}")
            throw wrapDohFailure(endpoint, host, query.type, error)
        }
    }

    private fun resolveLocal(host: String, type: Int): List<DnsAnswerRecord>? {
        config.findDnsHostOverride(host)?.let { override ->
            return resolveOverride(override, type)
        }
        return null
    }

    private fun resolveOverride(raw: String, type: Int): List<DnsAnswerRecord> {
        val value = raw.trim()
        if (value.startsWith("domain:")) {
            val alias = value.removePrefix("domain:").trim()
            if (alias.isEmpty()) {
                return emptyList()
            }
            return queryAlias(alias, type)
        }

        val ip = runCatching { InetAddress.getByName(value) }.getOrNull()
        if (ip != null) {
            return when {
                ip is Inet4Address && type == TYPE_A -> listOf(DnsAnswerRecord(TYPE_A, 60, ip.address))
                ip is Inet6Address && type == TYPE_AAAA -> listOf(DnsAnswerRecord(TYPE_AAAA, 60, ip.address))
                else -> emptyList()
            }
        }

        return queryAlias(value, type)
    }

    private fun queryAlias(alias: String, type: Int): List<DnsAnswerRecord> {
        val endpoint = primaryEndpoint ?: return emptyList()
        return try {
            queryDoh(endpoint, alias, type)
        } catch (_: Exception) {
            emptyList()
        }
    }

    private fun requirePrimaryEndpoint(): PreparedDohEndpoint {
        return primaryEndpoint ?: throw IOException("DoH 不可用，请自行更换 DoH")
    }

    private fun wrapDohFailure(
        endpoint: PreparedDohEndpoint,
        host: String,
        type: Int,
        error: Exception,
    ): IOException {
        val detail = error.message ?: error.javaClass.simpleName
        return IOException(
            "DoH 不可用，请自行更换 DoH。当前端点：${endpoint.rawUrl}，查询：$host ${queryTypeName(type)}，原因：$detail",
            error,
        )
    }

    private fun queryDoh(
        endpoint: PreparedDohEndpoint,
        host: String,
        type: Int,
    ): List<DnsAnswerRecord> {
        return if (type == TYPE_HTTPS || type == TYPE_SVCB || supportsDnsMessage(endpoint.url)) {
            queryDohDnsMessage(endpoint, host, type)
        } else {
            queryDohJson(endpoint, host, type)
        }
    }

    private fun queryDohJson(
        endpoint: PreparedDohEndpoint,
        host: String,
        type: Int,
    ): List<DnsAnswerRecord> {
        val resolvedUrl = endpoint.url.toURI().resolve(
            endpoint.url.path + "?name=$host&type=${queryTypeName(type)}"
        ).toURL()

        val request = Request.Builder()
            .url(resolvedUrl)
            .header("accept", "application/dns-json")
            .build()

        client.newCall(request).execute().use { response ->
            if (!response.isSuccessful) {
                throw IOException("DoH server ${endpoint.rawUrl} returned ${response.code}")
            }
            val body = response.body?.string().orEmpty()
            val payload = JSONObject(body)
            if (payload.optInt("Status", 0) != 0) {
                throw IOException("DoH status ${payload.optInt("Status")} from ${endpoint.rawUrl}")
            }

            val answers = payload.optJSONArray("Answer") ?: return emptyList()
            val records = ArrayList<DnsAnswerRecord>()
            for (index in 0 until answers.length()) {
                val answer = answers.getJSONObject(index)
                val answerType = answer.optInt("type")
                val ttl = answer.optInt("TTL", 60)
                val data = answer.optString("data")
                when (answerType) {
                    TYPE_A -> if (type == TYPE_A) {
                        val ip = InetAddress.getByName(data)
                        if (ip is Inet4Address) {
                            records += DnsAnswerRecord(TYPE_A, ttl, ip.address)
                        }
                    }
                    TYPE_AAAA -> if (type == TYPE_AAAA) {
                        val ip = InetAddress.getByName(data)
                        if (ip is Inet6Address) {
                            records += DnsAnswerRecord(TYPE_AAAA, ttl, ip.address)
                        }
                    }
                    TYPE_CNAME -> {
                        records += DnsAnswerRecord(TYPE_CNAME, ttl, DnsPacketCodec.encodeDomainName(data))
                    }
                }
            }
            return records
        }
    }

    private fun queryDohDnsMessage(
        endpoint: PreparedDohEndpoint,
        host: String,
        type: Int,
    ): List<DnsAnswerRecord> {
        val dnsQuery = DnsPacketCodec.buildWireQuery(host, type)
        val encoded = Base64.getUrlEncoder()
            .withoutPadding()
            .encodeToString(dnsQuery)
        val separator = if (endpoint.url.query.isNullOrEmpty()) "?" else "&"
        val resolvedUrl = URL(endpoint.rawUrl + separator + "dns=" + encoded)

        val request = Request.Builder()
            .url(resolvedUrl)
            .header("accept", "application/dns-message")
            .build()

        client.newCall(request).execute().use { response ->
            if (!response.isSuccessful) {
                throw IOException("DoH server ${endpoint.rawUrl} returned ${response.code}")
            }
            val body = response.body?.bytes() ?: throw IOException("empty DoH response from ${endpoint.rawUrl}")
            return DnsPacketCodec.parseWireResponse(body, type)
        }
    }

    private fun queryDohDnsMessageRaw(
        endpoint: PreparedDohEndpoint,
        payload: ByteArray,
        query: ParsedDnsQuery,
    ): DnsResolution {
        val encoded = Base64.getUrlEncoder()
            .withoutPadding()
            .encodeToString(payload)
        val separator = if (endpoint.url.query.isNullOrEmpty()) "?" else "&"
        val resolvedUrl = URL(endpoint.rawUrl + separator + "dns=" + encoded)

        val request = Request.Builder()
            .url(resolvedUrl)
            .header("accept", "application/dns-message")
            .build()

        client.newCall(request).execute().use { response ->
            if (!response.isSuccessful) {
                throw IOException("DoH server ${endpoint.rawUrl} returned ${response.code}")
            }
            val body = response.body?.bytes() ?: throw IOException("empty DoH response from ${endpoint.rawUrl}")
            return DnsResolution(DnsPacketCodec.parseWireResponse(body, query.type))
        }
    }

    private fun readCached(host: String, type: Int): DnsResolution? {
        val key = DnsCacheKey(host, type)
        val entry = cache[key] ?: return null
        if (SystemClock.elapsedRealtime() >= entry.expiresAtMs) {
            cache.remove(key)
            return null
        }
        return entry.resolution
    }

    private fun writeCache(host: String, type: Int, resolution: DnsResolution) {
        val minTtl = resolution.answers.minOfOrNull { it.ttl } ?: return
        if (minTtl <= 0) {
            cache.remove(DnsCacheKey(host, type))
            return
        }
        cache.entries.removeIf { (_, entry) -> SystemClock.elapsedRealtime() >= entry.expiresAtMs }
        cache[DnsCacheKey(host, type)] = CachedDnsResolution(
            resolution = resolution,
            expiresAtMs = SystemClock.elapsedRealtime() + (minTtl.toLong() * 1000L),
        )
    }

    private fun supportsDnsMessage(url: URL): Boolean {
        val path = url.path.lowercase(Locale.US)
        return !path.endsWith("/resolve")
    }

    companion object {
        const val TYPE_A = 1
        const val TYPE_CNAME = 5
        const val TYPE_AAAA = 28
        const val TYPE_SVCB = 64
        const val TYPE_HTTPS = 65

        fun isSupportedManagedType(type: Int): Boolean {
            return type == TYPE_A ||
                type == TYPE_AAAA ||
                type == TYPE_SVCB ||
                type == TYPE_HTTPS
        }

        fun isJsonFriendlyType(type: Int): Boolean {
            return type == TYPE_A || type == TYPE_AAAA
        }

        fun prepareEndpoints(rawEndpoints: List<String>): List<PreparedDohEndpoint> {
            return rawEndpoints.mapNotNull { raw ->
                val url = runCatching { URL(raw) }.getOrNull() ?: return@mapNotNull null
                val host = url.host.lowercase(Locale.US)
                val addresses = runCatching { InetAddress.getAllByName(host).toList() }.getOrNull()
                    ?: return@mapNotNull null
                PreparedDohEndpoint(raw, host, url, addresses)
            }
        }
    }
}

object DnsPacketCodec {
    fun buildWireQuery(domain: String, type: Int): ByteArray {
        val output = ByteArrayOutputStream()
        writeU16(output, 0)
        writeU16(output, 0x0100)
        writeU16(output, 1)
        writeU16(output, 0)
        writeU16(output, 0)
        writeU16(output, 0)
        output.write(encodeDomainName(domain))
        writeU16(output, type)
        writeU16(output, 1)
        return output.toByteArray()
    }

    fun parseDnsQuery(payload: ByteArray): ParsedDnsQuery? {
        if (payload.size < 12) {
            return null
        }
        val id = readU16(payload, 0)
        val flags = readU16(payload, 2)
        val qdCount = readU16(payload, 4)
        if (qdCount < 1) {
            return null
        }

        var offset = 12
        val labels = ArrayList<String>()
        while (offset < payload.size) {
            val length = payload[offset].toInt() and 0xff
            offset += 1
            if (length == 0) {
                break
            }
            if (length and 0xc0 != 0 || offset + length > payload.size) {
                return null
            }
            labels += payload.copyOfRange(offset, offset + length).decodeToString()
            offset += length
        }

        if (offset + 4 > payload.size) {
            return null
        }

        val questionEnd = offset + 4
        val questionBytes = payload.copyOfRange(12, questionEnd)
        return ParsedDnsQuery(
            id = id,
            flags = flags,
            name = labels.joinToString("."),
            type = readU16(payload, offset),
            questionBytes = questionBytes,
        )
    }

    fun buildResponse(query: ParsedDnsQuery, resolution: DnsResolution): ByteArray {
        val output = ByteArrayOutputStream()
        writeU16(output, query.id)
        val responseFlags = 0x8000 or (query.flags and 0x0100) or 0x0080 or (resolution.responseCode and 0x000f)
        writeU16(output, responseFlags)
        writeU16(output, 1)
        writeU16(output, resolution.answers.size)
        writeU16(output, 0)
        writeU16(output, 0)
        output.write(query.questionBytes)

        for (answer in resolution.answers) {
            writeU16(output, 0xc00c)
            writeU16(output, answer.type)
            writeU16(output, 1)
            writeU32(output, answer.ttl)
            writeU16(output, answer.data.size)
            output.write(answer.data)
        }

        return output.toByteArray()
    }

    fun encodeDomainName(domain: String): ByteArray {
        val output = ByteArrayOutputStream()
        val normalized = domain.trim().trimEnd('.')
        if (normalized.isEmpty()) {
            output.write(0)
            return output.toByteArray()
        }
        for (label in normalized.split('.')) {
            val bytes = label.toByteArray(Charsets.UTF_8)
            output.write(bytes.size)
            output.write(bytes)
        }
        output.write(0)
        return output.toByteArray()
    }

    fun parseWireResponse(payload: ByteArray, requestedType: Int): List<DnsAnswerRecord> {
        if (payload.size < 12) {
            throw IOException("short dns-message response")
        }
        val answerCount = readU16(payload, 6)
        var offset = 12
        val questionCount = readU16(payload, 4)
        repeat(questionCount) {
            offset = skipDnsName(payload, offset)
            if (offset + 4 > payload.size) {
                throw IOException("truncated dns question")
            }
            offset += 4
        }

        val records = ArrayList<DnsAnswerRecord>()
        repeat(answerCount) {
            offset = skipDnsName(payload, offset)
            if (offset + 10 > payload.size) {
                throw IOException("truncated dns answer")
            }
            val type = readU16(payload, offset)
            val ttl = readU32(payload, offset + 4)
            val dataLength = readU16(payload, offset + 8)
            offset += 10
            if (offset + dataLength > payload.size) {
                throw IOException("truncated dns rdata")
            }
            val rdata = payload.copyOfRange(offset, offset + dataLength)
            when (type) {
                LinuxdoDnsResolver.TYPE_A -> if (requestedType == LinuxdoDnsResolver.TYPE_A) {
                    records += DnsAnswerRecord(type, ttl, rdata)
                }
                LinuxdoDnsResolver.TYPE_AAAA -> if (requestedType == LinuxdoDnsResolver.TYPE_AAAA) {
                    records += DnsAnswerRecord(type, ttl, rdata)
                }
                LinuxdoDnsResolver.TYPE_CNAME -> {
                    val (cname, _) = readDnsName(payload, offset)
                    records += DnsAnswerRecord(type, ttl, encodeDomainName(cname))
                }
                else -> if (type == requestedType) {
                    records += DnsAnswerRecord(type, ttl, rdata)
                }
            }
            offset += dataLength
        }
        return records
    }

    private fun readU16(buffer: ByteArray, offset: Int): Int {
        return ((buffer[offset].toInt() and 0xff) shl 8) or (buffer[offset + 1].toInt() and 0xff)
    }

    private fun readU32(buffer: ByteArray, offset: Int): Int {
        return ((buffer[offset].toInt() and 0xff) shl 24) or
            ((buffer[offset + 1].toInt() and 0xff) shl 16) or
            ((buffer[offset + 2].toInt() and 0xff) shl 8) or
            (buffer[offset + 3].toInt() and 0xff)
    }

    private fun writeU16(output: ByteArrayOutputStream, value: Int) {
        output.write((value ushr 8) and 0xff)
        output.write(value and 0xff)
    }

    private fun writeU32(output: ByteArrayOutputStream, value: Int) {
        output.write((value ushr 24) and 0xff)
        output.write((value ushr 16) and 0xff)
        output.write((value ushr 8) and 0xff)
        output.write(value and 0xff)
    }

    private fun recordTypeName(type: Int): String = when (type) {
        LinuxdoDnsResolver.TYPE_A -> "A"
        LinuxdoDnsResolver.TYPE_AAAA -> "AAAA"
        else -> type.toString()
    }

    private fun skipDnsName(buffer: ByteArray, start: Int): Int {
        val (_, nextOffset) = readDnsName(buffer, start)
        return nextOffset
    }

    private fun readDnsName(buffer: ByteArray, start: Int): Pair<String, Int> {
        val labels = ArrayList<String>()
        var offset = start
        var jumped = false
        var nextOffset = start
        val visited = HashSet<Int>()

        while (offset < buffer.size) {
            if (!visited.add(offset)) {
                throw IOException("dns name compression loop")
            }
            val length = buffer[offset].toInt() and 0xff
            if (length == 0) {
                if (!jumped) {
                    nextOffset = offset + 1
                }
                break
            }
            if ((length and 0xc0) == 0xc0) {
                if (offset + 1 >= buffer.size) {
                    throw IOException("truncated dns compression pointer")
                }
                val pointer = ((length and 0x3f) shl 8) or (buffer[offset + 1].toInt() and 0xff)
                if (!jumped) {
                    nextOffset = offset + 2
                }
                offset = pointer
                jumped = true
                continue
            }
            val labelStart = offset + 1
            val labelEnd = labelStart + length
            if (labelEnd > buffer.size) {
                throw IOException("truncated dns label")
            }
            labels += buffer.copyOfRange(labelStart, labelEnd).decodeToString()
            offset = labelEnd
            if (!jumped) {
                nextOffset = offset
            }
        }

        return labels.joinToString(".") to nextOffset
    }
}

private fun queryTypeName(type: Int): String = when (type) {
    LinuxdoDnsResolver.TYPE_A -> "A"
    LinuxdoDnsResolver.TYPE_AAAA -> "AAAA"
    LinuxdoDnsResolver.TYPE_SVCB -> "SVCB"
    LinuxdoDnsResolver.TYPE_HTTPS -> "HTTPS"
    else -> type.toString()
}

data class UdpDnsPacket(
    val sourceIp: ByteArray,
    val destIp: ByteArray,
    val sourcePort: Int,
    val payload: ByteArray,
)

object TunPacketCodec {
    fun parseIpv4UdpDns(packet: ByteArray, length: Int, expectedDestIp: ByteArray): UdpDnsPacket? {
        if (length < 28) {
            return null
        }
        val version = (packet[0].toInt() ushr 4) and 0x0f
        val ihl = (packet[0].toInt() and 0x0f) * 4
        if (version != 4 || ihl < 20 || length < ihl + 8) {
            return null
        }
        val totalLength = ((packet[2].toInt() and 0xff) shl 8) or (packet[3].toInt() and 0xff)
        if (totalLength > length || packet[9].toInt() != 17) {
            return null
        }
        val destIp = packet.copyOfRange(16, 20)
        if (!destIp.contentEquals(expectedDestIp)) {
            return null
        }
        val udpOffset = ihl
        val destPort = ((packet[udpOffset + 2].toInt() and 0xff) shl 8) or (packet[udpOffset + 3].toInt() and 0xff)
        if (destPort != 53) {
            return null
        }
        val udpLength = ((packet[udpOffset + 4].toInt() and 0xff) shl 8) or (packet[udpOffset + 5].toInt() and 0xff)
        if (udpLength < 8 || udpOffset + udpLength > totalLength) {
            return null
        }
        return UdpDnsPacket(
            sourceIp = packet.copyOfRange(12, 16),
            destIp = destIp,
            sourcePort = ((packet[udpOffset].toInt() and 0xff) shl 8) or (packet[udpOffset + 1].toInt() and 0xff),
            payload = packet.copyOfRange(udpOffset + 8, udpOffset + udpLength),
        )
    }

    fun buildIpv4UdpResponse(request: UdpDnsPacket, responsePayload: ByteArray): ByteArray {
        val ipHeaderLength = 20
        val udpHeaderLength = 8
        val totalLength = ipHeaderLength + udpHeaderLength + responsePayload.size
        val packet = ByteArray(totalLength)
        packet[0] = 0x45
        packet[1] = 0
        packet[2] = (totalLength ushr 8).toByte()
        packet[3] = totalLength.toByte()
        packet[4] = 0
        packet[5] = 0
        packet[6] = 0
        packet[7] = 0
        packet[8] = 64
        packet[9] = 17
        request.destIp.copyInto(packet, 12)
        request.sourceIp.copyInto(packet, 16)
        writeU16(packet, 20, 53)
        writeU16(packet, 22, request.sourcePort)
        writeU16(packet, 24, udpHeaderLength + responsePayload.size)
        writeU16(packet, 26, 0)
        responsePayload.copyInto(packet, 28)
        writeU16(packet, 10, ipv4Checksum(packet, 0, ipHeaderLength))
        return packet
    }

    private fun writeU16(buffer: ByteArray, offset: Int, value: Int) {
        buffer[offset] = (value ushr 8).toByte()
        buffer[offset + 1] = value.toByte()
    }

    private fun ipv4Checksum(buffer: ByteArray, offset: Int, length: Int): Int {
        var sum = 0
        var index = offset
        while (index < offset + length) {
            if (index == offset + 10) {
                index += 2
                continue
            }
            sum += ((buffer[index].toInt() and 0xff) shl 8) or (buffer[index + 1].toInt() and 0xff)
            while (sum > 0xffff) {
                sum = (sum and 0xffff) + (sum ushr 16)
            }
            index += 2
        }
        return sum.inv() and 0xffff
    }
}

private class StaticDns(
    private val overrides: Map<String, List<InetAddress>>,
) : Dns {
    override fun lookup(hostname: String): List<InetAddress> {
        return overrides[hostname.lowercase(Locale.US)] ?: Dns.SYSTEM.lookup(hostname)
    }
}
