package io.linuxdo.accelerator.android

import android.content.Context
import org.tomlj.Toml
import org.tomlj.TomlArray
import org.tomlj.TomlTable
import java.io.File
import java.io.IOException
import java.net.InetAddress
import java.util.Locale

data class LinuxdoConfig(
    val listenHost: String,
    val dohEndpoints: List<String>,
    val preferManagedIpv6: Boolean,
    val dnsHosts: Map<String, String>,
    val proxyDomains: List<String>,
) {
    fun shouldUseManagedDoh(host: String): Boolean = matchesProxyHost(host)

    fun isManagedHost(host: String): Boolean {
        val candidate = host.lowercase(Locale.US)
        return findDnsHostOverride(candidate) != null || matchesProxyHost(candidate)
    }

    fun matchesProxyHost(host: String): Boolean {
        val candidate = host.lowercase(Locale.US)
        return proxyDomains.any { pattern ->
            val normalized = pattern.lowercase(Locale.US)
            if (normalized.startsWith("*.")) {
                candidate.endsWith(".${normalized.removePrefix("*.")}")
            } else {
                candidate == normalized
            }
        }
    }

    fun findDnsHostOverride(host: String): String? {
        val candidate = host.lowercase(Locale.US)
        dnsHosts[candidate]?.let { return it }

        return dnsHosts.entries
            .mapNotNull { (pattern, target) ->
                val normalized = pattern.lowercase(Locale.US)
                if (!normalized.startsWith("*.")) {
                    null
                } else {
                    val suffix = normalized.removePrefix("*.")
                    if (candidate.endsWith(".$suffix")) suffix.length to target else null
                }
            }
            .maxByOrNull { it.first }
            ?.second
    }

    fun localListenAddress(): InetAddress = InetAddress.getByName(listenHost)

    companion object {
        fun fromTomlFile(file: File): LinuxdoConfig {
            val parsed = Toml.parse(file.toPath())
            if (parsed.hasErrors()) {
                throw IOException(parsed.errors().joinToString("; ") { it.toString() })
            }

            return LinuxdoConfig(
                listenHost = parsed.getString("listen_host")
                    ?: throw IOException("listen_host is missing"),
                dohEndpoints = parsed.getArray("doh_endpoints")?.toStringList().orEmpty(),
                preferManagedIpv6 = parsed.getBoolean("managed_prefer_ipv6") ?: true,
                dnsHosts = parsed.getTable("dns_hosts")?.toStringMap().orEmpty(),
                proxyDomains = parsed.getArray("proxy_domains")?.toStringList().orEmpty(),
            )
        }
    }
}

data class CommandResult(
    val exitCode: Int,
    val stdout: String,
    val stderr: String,
) {
    fun requireSuccess(action: String): CommandResult {
        if (exitCode != 0) {
            val detail = stderr.ifBlank { stdout }.ifBlank { "unknown failure" }
            throw IOException("$action failed: $detail")
        }
        return this
    }
}

object LinuxdoBinary {
    private const val BINARY_ASSET = "bin/linuxdo-accelerator"
    private const val CONFIG_ASSET = "defaults/linuxdo-accelerator.toml"
    private const val BINARY_NAME = "linuxdo-accelerator"
    private const val CONFIG_NAME = "linuxdo-accelerator.toml"
    private const val ROOT_BINARY_DIR = "/data/local/tmp/linuxdo-accelerator"
    private const val ROOT_BINARY_PATH = "$ROOT_BINARY_DIR/$BINARY_NAME"
    private const val SHELL_PATH = "/system/bin/sh"
    private val SU_CANDIDATES = listOf("/system/bin/su", "/system/xbin/su", "su")

    fun ensureAssets(context: Context) {
        val binaryFile = stagedBinaryFile(context)
        val configFile = ensureConfigFile(context)

        binaryFile.parentFile?.mkdirs()
        copyAsset(context, BINARY_ASSET, binaryFile, overwrite = true)
    }

    fun configFile(context: Context): File {
        val directory = userEditableConfigDir(context)
        directory.mkdirs()
        return File(directory, CONFIG_NAME)
    }

    fun ensureConfigFile(context: Context): File {
        val configFile = configFile(context)
        val markerFile = File(configFile.parentFile, "$CONFIG_NAME.version")
        val backupFile = File(configFile.parentFile, "$CONFIG_NAME.bak")
        val currentVersion = BuildConfig.GIT_HASH

        if (configFile.exists()) {
            val markerVersion = readVersionMarker(markerFile)
            if (markerVersion != currentVersion) {
                configFile.copyTo(backupFile, overwrite = true)
                copyAsset(context, CONFIG_ASSET, configFile, overwrite = true)
                writeVersionMarker(markerFile, currentVersion)
            }
            return configFile
        }

        val legacyFile = File(context.filesDir, CONFIG_NAME)
        if (legacyFile.exists()) {
            legacyFile.copyTo(configFile, overwrite = true)
            writeVersionMarker(markerFile, currentVersion)
            return configFile
        }

        copyAsset(context, CONFIG_ASSET, configFile, overwrite = false)
        writeVersionMarker(markerFile, currentVersion)
        return configFile
    }

    private fun readVersionMarker(markerFile: File): String? {
        if (!markerFile.exists()) return null
        return markerFile.readText().trim().ifBlank { null }
    }

    private fun writeVersionMarker(markerFile: File, version: String) {
        markerFile.parentFile?.mkdirs()
        markerFile.writeText(version)
    }

    fun readConfig(context: Context): LinuxdoConfig = LinuxdoConfig.fromTomlFile(configFile(context))

    fun runRoot(context: Context, vararg args: String): CommandResult = run(context, *args)

    private fun run(context: Context, vararg args: String): CommandResult {
        ensureAssets(context)
        deployRootBinary(context)
        val command = buildList {
            add(ROOT_BINARY_PATH)
            add("--config")
            add(configFile(context).absolutePath)
            addAll(args)
        }

        val process = startSuProcess(shellJoin(command))

        val stdout = process.inputStream.bufferedReader().use { it.readText().trim() }
        val stderr = process.errorStream.bufferedReader().use { it.readText().trim() }
        val exitCode = process.waitFor()
        return CommandResult(exitCode, stdout, stderr)
    }

    private fun deployRootBinary(context: Context) {
        val stagedBinary = stagedBinaryFile(context)
        val command = listOf(
            "mkdir -p ${shellQuote(ROOT_BINARY_DIR)}",
            "cp ${shellQuote(stagedBinary.absolutePath)} ${shellQuote(ROOT_BINARY_PATH)}",
            "chmod 755 ${shellQuote(ROOT_BINARY_PATH)}",
        ).joinToString(" && ")

        val process = startSuProcess(command)
        val stdout = process.inputStream.bufferedReader().use { it.readText().trim() }
        val stderr = process.errorStream.bufferedReader().use { it.readText().trim() }
        val exitCode = process.waitFor()
        if (exitCode != 0) {
            val detail = stderr.ifBlank { stdout }.ifBlank { "unknown failure" }
            throw IOException("deploy root binary failed: $detail")
        }
    }

    private fun stagedBinaryFile(context: Context): File = File(File(context.filesDir, "bin"), BINARY_NAME)

    private fun userEditableConfigDir(context: Context): File {
        val mediaDir = context.externalMediaDirs
            .firstOrNull { candidate -> candidate != null && candidate.absolutePath.contains("/Android/media/") }
        if (mediaDir != null) {
            return mediaDir
        }

        context.getExternalFilesDir(null)?.let { return it }
        return context.filesDir
    }

    private fun copyAsset(context: Context, assetName: String, target: File, overwrite: Boolean) {
        if (target.exists() && !overwrite) {
            return
        }
        target.parentFile?.mkdirs()
        context.assets.open(assetName).use { input ->
            target.outputStream().use { output ->
                input.copyTo(output)
            }
        }
    }

    private fun shellJoin(args: List<String>): String = args.joinToString(" ") { shellQuote(it) }

    private fun shellQuote(value: String): String = "'" + value.replace("'", "'\"'\"'") + "'"

    private fun startSuProcess(command: String): Process {
        val shellScript = buildString {
            append("SU_BIN=''")
            for (candidate in SU_CANDIDATES) {
                if (candidate.contains("/")) {
                    append("; if [ -z \"${'$'}SU_BIN\" ] && [ -x ")
                    append(shellQuote(candidate))
                    append(" ]; then SU_BIN=")
                    append(shellQuote(candidate))
                    append("; fi")
                } else {
                    append("; if [ -z \"${'$'}SU_BIN\" ] && command -v ")
                    append(shellQuote(candidate))
                    append(" >/dev/null 2>&1; then SU_BIN=${'$'}(command -v ")
                    append(shellQuote(candidate))
                    append("); fi")
                }
            }
            append("; if [ -z \"${'$'}SU_BIN\" ]; then echo 'su not found in shell PATH' >&2; exit 127; fi")
            append("; exec \"${'$'}SU_BIN\" -c ")
            append(shellQuote(command))
        }

        return try {
            ProcessBuilder(SHELL_PATH, "-c", shellScript)
                .redirectErrorStream(false)
                .start()
        } catch (error: IOException) {
            throw IOException("failed to launch $SHELL_PATH: ${error.message ?: error.javaClass.simpleName}")
        }
    }
}

private fun TomlArray.toStringList(): List<String> = buildList(size().toInt()) {
    for (index in 0 until size()) {
        add(getString(index) ?: "")
    }
}

private fun TomlTable.toStringMap(): Map<String, String> = buildMap {
    for (key in keySet()) {
        put(key, getString(key) ?: "")
    }
}
