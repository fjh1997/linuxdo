package io.linuxdo.accelerator.android

import android.graphics.drawable.Icon
import android.content.Intent
import android.net.VpnService
import android.os.Build
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import androidx.core.content.ContextCompat

class LinuxdoTileService : TileService() {
    override fun onStartListening() {
        super.onStartListening()
        refreshTile()
    }

    override fun onClick() {
        super.onClick()
        val (running, _, _) = LinuxdoVpnService.readCurrentStatus(this)
        if (running) {
            startService(Intent(this, LinuxdoVpnService::class.java).setAction(LinuxdoVpnService.ACTION_STOP))
            refreshTile()
            return
        }

        val permissionIntent = VpnService.prepare(this)
        if (permissionIntent != null) {
            launchMainActivity(requestStart = true)
            return
        }

        ContextCompat.startForegroundService(
            this,
            Intent(this, LinuxdoVpnService::class.java).setAction(LinuxdoVpnService.ACTION_START),
        )
        refreshTile()
    }

    private fun refreshTile() {
        val tile = qsTile ?: return
        val (state, subtitle) = LinuxdoVpnService.currentTileState(this)
        tile.state = state
        tile.label = getString(R.string.app_name)
        tile.icon = Icon.createWithResource(this, R.drawable.ic_linuxdo_logo)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            tile.subtitle = subtitle
        }
        tile.updateTile()
    }

    private fun launchMainActivity(requestStart: Boolean) {
        val intent = Intent(this, MainActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        if (requestStart) {
            intent.putExtra(MainActivity.EXTRA_REQUEST_START, true)
        }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startActivityAndCollapse(intent)
        } else {
            @Suppress("DEPRECATION")
            startActivityAndCollapse(intent)
        }
    }
}
