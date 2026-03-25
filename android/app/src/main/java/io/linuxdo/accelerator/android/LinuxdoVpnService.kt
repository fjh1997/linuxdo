package io.linuxdo.accelerator.android

import android.app.ActivityManager
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.net.VpnService
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.ParcelFileDescriptor
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import android.util.Log
import androidx.core.app.NotificationCompat
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.IOException
import java.net.Inet6Address
import java.net.InetAddress
import java.net.InetSocketAddress
import java.util.Locale
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

class LinuxdoVpnService : VpnService() {
    private val tag = "LinuxdoVpnService"
    private var vpnInterface: ParcelFileDescriptor? = null
    private var workerThread: Thread? = null
    private val running = AtomicBoolean(false)
    @Volatile
    private var preferManagedIpv6Status = false
    @Volatile
    private var activeDohEndpoint = "未配置"
    @Volatile
    private var lastDohFailureDetail: String? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                stopAccelerator("已停止")
                return START_NOT_STICKY
            }
            ACTION_START -> {
                if (running.get()) {
                    broadcastStatus(true, currentRunningStatusText(), currentRunningDetail())
                    return START_STICKY
                }
                ensureNotificationChannel()
                val notification = buildNotification(getString(R.string.notification_text_running))
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                    startForeground(
                        NOTIFICATION_ID,
                        notification,
                        ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
                    )
                } else {
                    startForeground(NOTIFICATION_ID, notification)
                }
                startAcceleratorAsync()
                return START_STICKY
            }
        }
        return START_NOT_STICKY
    }

    override fun onTaskRemoved(rootIntent: Intent?) {
        super.onTaskRemoved(rootIntent)
        if (running.get()) {
            broadcastStatus(true, currentRunningStatusText(), currentRunningDetail())
        } else {
            broadcastStatus(false, "未启动", null)
        }
    }

    override fun onDestroy() {
        if (running.get() || vpnInterface != null) {
            stopAccelerator("服务已销毁")
        }
        super.onDestroy()
    }

    override fun onRevoke() {
        stopAccelerator("VPN 权限已撤销")
        super.onRevoke()
    }

    private fun startAcceleratorAsync() {
        workerThread = thread(name = "linuxdo-vpn-start") {
            try {
                Log.i(tag, "starting accelerator")
                LinuxdoBinary.ensureAssets(this)
                val config = LinuxdoBinary.readConfig(this)
                val preparedEndpoints = LinuxdoDnsResolver.prepareEndpoints(config.dohEndpoints)
                if (preparedEndpoints.isEmpty()) {
                    throw IOException("没有可用的 DoH 端点")
                }
                val systemDnsServers = resolveSystemDnsServers()
                val preferManagedIpv6 = config.preferManagedIpv6 && hasUsableIpv6Network()
                Log.i(tag, "system dns servers=$systemDnsServers")

                val vpn = Builder()
                    .setSession("Linux.do Accelerator")
                    .setMtu(1500)
                    .addAddress(VPN_CLIENT_IP, 24)
                    .addRoute(VPN_DNS_IP, 32)
                    .addDnsServer(VPN_DNS_IP)
                    .establish()
                    ?: throw IOException("failed to establish VPN interface")

                vpnInterface = vpn
                running.set(true)
                preferManagedIpv6Status = preferManagedIpv6
                activeDohEndpoint = preparedEndpoints.firstOrNull()?.rawUrl ?: "未配置"
                lastDohFailureDetail = null
                Log.i(tag, "vpn established, first DoH=$activeDohEndpoint")
                broadcastStatus(true, currentRunningStatusText(), currentRunningDetail())
                runDnsLoop(vpn, config, preparedEndpoints, systemDnsServers)
            } catch (error: Exception) {
                Log.e(tag, "startAcceleratorAsync failed", error)
                running.set(false)
                safeClose(vpnInterface)
                vpnInterface = null
                broadcastStatus(false, "启动失败", error.message ?: error.toString())
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
            }
        }
    }

    private fun runDnsLoop(
        vpn: ParcelFileDescriptor,
        config: LinuxdoConfig,
        endpoints: List<PreparedDohEndpoint>,
        systemDnsServers: List<InetAddress>,
    ) {
        val dnsServerIp = InetAddress.getByName(VPN_DNS_IP).address
        val resolver = LinuxdoDnsResolver(config, endpoints)
        val input = FileInputStream(vpn.fileDescriptor)
        val output = FileOutputStream(vpn.fileDescriptor)
        val buffer = ByteArray(32767)

        try {
            while (running.get()) {
                val length = input.read(buffer)
                if (length <= 0) {
                    continue
                }
                val requestPacket = TunPacketCodec.parseIpv4UdpDns(buffer, length, dnsServerIp) ?: continue
                val query = DnsPacketCodec.parseDnsQuery(requestPacket.payload) ?: continue
                val responsePayload = if (shouldUseManagedDoh(config, query)) {
                    try {
                        val payload = resolver.resolveManagedPayload(requestPacket.payload, query)
                        clearDohFailure()
                        payload
                    } catch (error: Exception) {
                        Log.w(tag, "managed dns query failed for ${query.name} type=${query.type}: ${error.message}")
                        reportDohFailure(query, error)
                        DnsPacketCodec.buildResponse(query, DnsResolution(emptyList(), responseCode = 2))
                    }
                } else {
                    forwardSystemDns(requestPacket.payload, systemDnsServers)
                        ?: DnsPacketCodec.buildResponse(query, DnsResolution(emptyList(), responseCode = 2))
                }
                val responsePacket = TunPacketCodec.buildIpv4UdpResponse(requestPacket, responsePayload)
                output.write(responsePacket)
                output.flush()
            }
        } catch (error: IOException) {
            Log.e(tag, "runDnsLoop failed", error)
        } finally {
            Log.i(tag, "runDnsLoop exiting")
            running.set(false)
            safeClose(vpn)
            vpnInterface = null
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        }
    }

    private fun shouldUseManagedDoh(config: LinuxdoConfig, query: ParsedDnsQuery): Boolean {
        return config.shouldUseManagedDoh(query.name)
    }

    private fun currentRunningStatusText(): String {
        return if (lastDohFailureDetail == null) "加速中" else "加速中（DoH 异常）"
    }

    private fun currentRunningDetail(): String {
        return lastDohFailureDetail ?: buildHealthyRunningDetail()
    }

    private fun buildHealthyRunningDetail(): String {
        val summary = if (preferManagedIpv6Status) {
            "仅 linux.do / *.linux.do 走自定义 DoH，并保留 AAAA 返回；其他域名走系统默认 DNS。"
        } else {
            "仅 linux.do / *.linux.do 走自定义 DoH；其他域名走系统默认 DNS。"
        }
        return "$summary 当前 DoH：$activeDohEndpoint"
    }

    private fun clearDohFailure() {
        if (lastDohFailureDetail == null || !running.get()) {
            return
        }
        lastDohFailureDetail = null
        broadcastStatus(true, currentRunningStatusText(), currentRunningDetail())
    }

    private fun reportDohFailure(query: ParsedDnsQuery, error: Exception) {
        val detail = buildString {
            append("DoH 查询失败：")
            append(query.name.lowercase(Locale.US))
            append(' ')
            append(queryTypeName(query.type))
            append("。当前 DoH：")
            append(activeDohEndpoint)
            append("。原因：")
            append(error.message ?: error.javaClass.simpleName)
            append("。请自行更换 DoH；其他域名仍走系统默认 DNS。")
        }
        if (detail == lastDohFailureDetail || !running.get()) {
            return
        }
        lastDohFailureDetail = detail
        broadcastStatus(true, currentRunningStatusText(), detail)
    }

    private fun forwardSystemDns(payload: ByteArray, servers: List<InetAddress>): ByteArray? {
        for (server in servers) {
            try {
                DatagramSocket().use { socket ->
                    if (!protect(socket)) {
                        Log.w(tag, "protect() failed for DNS forward socket")
                    }
                    socket.soTimeout = 3000
                    socket.connect(InetSocketAddress(server, 53))
                    socket.send(DatagramPacket(payload, payload.size))

                    val buffer = ByteArray(4096)
                    val packet = DatagramPacket(buffer, buffer.size)
                    socket.receive(packet)
                    return packet.data.copyOf(packet.length)
                }
            } catch (error: Exception) {
                Log.w(tag, "system dns forward failed via $server: ${error.message}")
            }
        }
        return null
    }

    private fun resolveSystemDnsServers(): List<InetAddress> {
        val connectivityManager = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        return connectivityManager.allNetworks
            .mapNotNull { network ->
                val capabilities = connectivityManager.getNetworkCapabilities(network) ?: return@mapNotNull null
                if (capabilities.hasTransport(NetworkCapabilities.TRANSPORT_VPN)) {
                    return@mapNotNull null
                }
                connectivityManager.getLinkProperties(network)?.dnsServers
            }
            .flatten()
            .distinct()
    }

    private fun hasUsableIpv6Network(): Boolean {
        val connectivityManager = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        return connectivityManager.allNetworks.any { network ->
            val capabilities = connectivityManager.getNetworkCapabilities(network) ?: return@any false
            if (capabilities.hasTransport(NetworkCapabilities.TRANSPORT_VPN)) {
                return@any false
            }
            val linkProperties = connectivityManager.getLinkProperties(network) ?: return@any false
            linkProperties.linkAddresses.any { linkAddress ->
                val address = linkAddress.address
                address is Inet6Address && !address.isLinkLocalAddress && !address.isLoopbackAddress
            }
        }
    }

    private fun stopAccelerator(statusText: String) {
        running.set(false)
        safeClose(vpnInterface)
        vpnInterface = null
        lastDohFailureDetail = null
        broadcastStatus(false, statusText, "Android VPN DNS 接管已关闭。")
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    private fun buildNotification(text: String) = NotificationCompat.Builder(this, NOTIFICATION_CHANNEL_ID)
        .setContentTitle(getString(R.string.notification_title))
        .setContentText(text)
        .setSmallIcon(R.drawable.ic_notification)
        .setCategory(NotificationCompat.CATEGORY_SERVICE)
        .setPriority(NotificationCompat.PRIORITY_LOW)
        .setOngoing(true)
        .setContentIntent(
            PendingIntent.getActivity(
                this,
                1,
                Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
        )
        .build()

    private fun broadcastStatus(running: Boolean, statusText: String, detail: String?) {
        saveStatusSnapshot(running, statusText, detail)
        ensureNotificationChannel()
        requestTileRefresh()
        sendBroadcast(
            Intent(ACTION_STATUS).apply {
                setPackage(packageName)
                putExtra(EXTRA_RUNNING, running)
                putExtra(EXTRA_STATUS, statusText)
                putExtra(EXTRA_DETAIL, detail)
            }
        )
    }

    private fun ensureNotificationChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
            return
        }
        val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val channel = NotificationChannel(
            NOTIFICATION_CHANNEL_ID,
            "Linux.do Accelerator",
            NotificationManager.IMPORTANCE_LOW,
        )
        manager.createNotificationChannel(channel)
    }

    private fun requestTileRefresh() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.N) {
            return
        }
        TileService.requestListeningState(
            this,
            ComponentName(this, LinuxdoTileService::class.java),
        )
    }

    private fun safeClose(value: ParcelFileDescriptor?) {
        try {
            value?.close()
        } catch (_: IOException) {
        }
    }

    private fun saveStatusSnapshot(running: Boolean, statusText: String, detail: String?) {
        getSharedPreferences(STATE_PREFS, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(PREF_RUNNING, running)
            .putString(PREF_STATUS, statusText)
            .putString(PREF_DETAIL, detail)
            .apply()
    }

    companion object {
        const val ACTION_START = "io.linuxdo.accelerator.android.action.START"
        const val ACTION_STOP = "io.linuxdo.accelerator.android.action.STOP"
        const val ACTION_STATUS = "io.linuxdo.accelerator.android.action.STATUS"
        const val EXTRA_RUNNING = "extra_running"
        const val EXTRA_STATUS = "extra_status"
        const val EXTRA_DETAIL = "extra_detail"

        private const val STATE_PREFS = "linuxdo_android_state"
        private const val PREF_RUNNING = "running"
        private const val PREF_STATUS = "status"
        private const val PREF_DETAIL = "detail"
        private const val NOTIFICATION_CHANNEL_ID = "linuxdo-accelerator"
        private const val NOTIFICATION_ID = 101
        private const val VPN_CLIENT_IP = "10.77.0.1"
        private const val VPN_DNS_IP = "10.77.0.2"

        fun readSavedStatus(context: Context): Triple<Boolean, String, String> {
            val prefs = context.getSharedPreferences(STATE_PREFS, Context.MODE_PRIVATE)
            return Triple(
                prefs.getBoolean(PREF_RUNNING, false),
                prefs.getString(PREF_STATUS, "未启动").orEmpty(),
                prefs.getString(PREF_DETAIL, "配置文件和 Android 壳已就绪。").orEmpty(),
            )
        }

        fun readCurrentStatus(context: Context): Triple<Boolean, String, String> {
            val prefs = context.getSharedPreferences(STATE_PREFS, Context.MODE_PRIVATE)
            val savedRunning = prefs.getBoolean(PREF_RUNNING, false)
            val savedStatus = prefs.getString(PREF_STATUS, "未启动").orEmpty()
            val savedDetail = prefs.getString(PREF_DETAIL, "配置文件和 Android 壳已就绪。").orEmpty()
            val actualRunning = isServiceAlive(context) || isServiceNotificationActive(context)

            if (actualRunning) {
                val status = if (savedStatus.isBlank() || savedStatus == "未启动") "加速中" else savedStatus
                val detail = if (savedDetail.isBlank()) {
                    "仅 linux.do / *.linux.do 走自定义 DoH；其他域名走系统默认 DNS。"
                } else {
                    savedDetail
                }
                if (!savedRunning || savedStatus != status || savedDetail != detail) {
                    prefs.edit()
                        .putBoolean(PREF_RUNNING, true)
                        .putString(PREF_STATUS, status)
                        .putString(PREF_DETAIL, detail)
                        .apply()
                }
                return Triple(true, status, detail)
            }

            if (!savedRunning && savedStatus == "服务已销毁") {
                val status = "已停止"
                val detail = if (savedDetail.isBlank()) {
                    "Android VPN DNS 接管已关闭。"
                } else {
                    savedDetail
                }
                prefs.edit()
                    .putBoolean(PREF_RUNNING, false)
                    .putString(PREF_STATUS, status)
                    .putString(PREF_DETAIL, detail)
                    .apply()
                return Triple(false, status, detail)
            }

            if (!savedRunning) {
                return Triple(false, savedStatus, savedDetail)
            }

            val detail = "Android VPN 服务当前不在运行，可能已被系统结束或手动关闭。"
            prefs.edit()
                .putBoolean(PREF_RUNNING, false)
                .putString(PREF_STATUS, "未启动")
                .putString(PREF_DETAIL, detail)
                .apply()
            return Triple(false, "未启动", detail)
        }

        private fun isServiceAlive(context: Context): Boolean {
            val manager = context.getSystemService(Context.ACTIVITY_SERVICE) as? ActivityManager ?: return false
            @Suppress("DEPRECATION")
            return manager.getRunningServices(Int.MAX_VALUE)
                .any { it.service.className == LinuxdoVpnService::class.java.name }
        }

        private fun isServiceNotificationActive(context: Context): Boolean {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) {
                return false
            }
            val manager = context.getSystemService(Context.NOTIFICATION_SERVICE) as? NotificationManager
                ?: return false
            return manager.activeNotifications.any {
                it.id == NOTIFICATION_ID && it.packageName == context.packageName
            }
        }

        fun requestTileRefresh(context: Context) {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.N) {
                return
            }
            TileService.requestListeningState(
                context,
                ComponentName(context, LinuxdoTileService::class.java),
            )
        }

        fun currentTileState(context: Context): Pair<Int, String> {
            val (running, _, detail) = readCurrentStatus(context)
            return if (running) {
                Tile.STATE_ACTIVE to context.getString(R.string.tile_subtitle_running)
            } else {
                Tile.STATE_INACTIVE to detail
            }
        }
    }
}
