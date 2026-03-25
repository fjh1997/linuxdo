package io.linuxdo.accelerator.android

import android.app.Activity
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.VpnService
import android.os.Build
import android.os.Bundle
import android.view.View
import android.widget.Button
import android.widget.TextView
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import kotlin.concurrent.thread

class MainActivity : AppCompatActivity() {
    private lateinit var contentLayout: View
    private lateinit var toggleButton: Button
    private lateinit var statusView: TextView
    private lateinit var detailView: TextView
    private lateinit var configPathView: TextView

    private var isRunning = false

    private val vpnPermissionLauncher =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
            if (result.resultCode == Activity.RESULT_OK) {
                startVpnService()
            } else {
                renderState(false, "未授予 VPN 权限", "系统没有同意建立 Android VPN 接管。")
            }
        }

    private val statusReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            if (intent?.action != LinuxdoVpnService.ACTION_STATUS) {
                return
            }
            renderState(
                running = intent.getBooleanExtra(LinuxdoVpnService.EXTRA_RUNNING, false),
                status = intent.getStringExtra(LinuxdoVpnService.EXTRA_STATUS).orEmpty(),
                detail = intent.getStringExtra(LinuxdoVpnService.EXTRA_DETAIL).orEmpty(),
            )
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        contentLayout = findViewById(R.id.contentLayout)
        toggleButton = findViewById(R.id.toggleButton)
        statusView = findViewById(R.id.statusView)
        detailView = findViewById(R.id.detailView)
        configPathView = findViewById(R.id.configPathView)

        val baseTopPadding = contentLayout.paddingTop
        ViewCompat.setOnApplyWindowInsetsListener(contentLayout) { view, insets ->
            val systemBars = insets.getInsets(WindowInsetsCompat.Type.systemBars())
            view.setPadding(
                view.paddingLeft,
                baseTopPadding + systemBars.top,
                view.paddingRight,
                view.paddingBottom,
            )
            insets
        }
        ViewCompat.requestApplyInsets(contentLayout)

        try {
            configPathView.text = LinuxdoBinary.ensureConfigFile(this).absolutePath
        } catch (error: Exception) {
            configPathView.text = LinuxdoBinary.configFile(this).absolutePath
            detailView.text = error.message ?: error.toString()
        }
        toggleButton.setOnClickListener {
            if (isRunning) {
                stopVpnService()
            } else {
                requestVpnPermissionAndStart()
            }
        }

        handleIntent(intent)
    }

    override fun onNewIntent(intent: Intent?) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleIntent(intent)
    }

    override fun onStart() {
        super.onStart()
        registerStatusReceiver()
        val (running, status, detail) = LinuxdoVpnService.readCurrentStatus(this)
        val normalizedStatus = if (!running && status == "服务已销毁") "已停止" else status
        renderState(
            running,
            normalizedStatus,
            if (detail.isBlank()) {
                "配置文件和 Android 壳已就绪。启动后仅 linux.do / *.linux.do 走自定义 DoH，其他域名仍走系统默认 DNS。"
            } else {
                detail
            },
        )
    }

    override fun onStop() {
        unregisterReceiver(statusReceiver)
        super.onStop()
    }

    private fun requestVpnPermissionAndStart() {
        thread(name = "linuxdo-prepare-assets") {
            try {
                LinuxdoBinary.ensureAssets(this)
                val intent = VpnService.prepare(this)
                runOnUiThread {
                    if (intent == null) {
                        startVpnService()
                    } else {
                        vpnPermissionLauncher.launch(intent)
                    }
                }
            } catch (error: Exception) {
                runOnUiThread {
                    renderState(false, "准备失败", error.message ?: error.toString())
                }
            }
        }
    }

    private fun startVpnService() {
        renderState(false, "正在启动", "正在建立 Android VPN，并启用 linux.do / *.linux.do 的自定义 DoH 接管...")
        ContextCompat.startForegroundService(
            this,
            Intent(this, LinuxdoVpnService::class.java).setAction(LinuxdoVpnService.ACTION_START),
        )
    }

    private fun stopVpnService() {
        startService(Intent(this, LinuxdoVpnService::class.java).setAction(LinuxdoVpnService.ACTION_STOP))
    }

    private fun handleIntent(intent: Intent?) {
        if (intent?.getBooleanExtra(EXTRA_REQUEST_START, false) == true && !isRunning) {
            intent.removeExtra(EXTRA_REQUEST_START)
            requestVpnPermissionAndStart()
        }
    }

    private fun renderState(running: Boolean, status: String, detail: String) {
        isRunning = running
        statusView.text = status
        detailView.text = detail
        toggleButton.text = if (running) getString(R.string.action_stop) else getString(R.string.action_start)
        toggleButton.alpha = 1.0f
    }

    private fun registerStatusReceiver() {
        val filter = IntentFilter(LinuxdoVpnService.ACTION_STATUS)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(statusReceiver, filter, RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("DEPRECATION")
            registerReceiver(statusReceiver, filter)
        }
    }

    companion object {
        const val EXTRA_REQUEST_START = "request_start"
    }
}
