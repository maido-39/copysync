package com.copysync.android.ui

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.Settings as AndroidSettings
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import com.copysync.android.data.HistoryDb
import com.copysync.android.data.Secrets
import com.copysync.android.data.Settings
import com.copysync.android.net.claimAndStore
import com.copysync.android.sync.SyncService
import com.copysync.android.sync.SyncState
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

@Composable
fun AppRoot() {
    val ctx = LocalContext.current
    var paired by remember { mutableStateOf(Settings(ctx).isPaired) }
    if (paired) {
        StatusScreen(onUnpair = { paired = false })
    } else {
        PairingScreen(onPaired = { paired = true })
    }
}

@Composable
private fun PairingScreen(onPaired: () -> Unit) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    var server by remember { mutableStateOf("https://10.0.2.2:8443") }
    var otp by remember { mutableStateOf("") }
    var name by remember { mutableStateOf("android") }
    var pin by remember { mutableStateOf("") }
    var msg by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }

    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Text("Pair this device", style = MaterialTheme.typography.headlineSmall)
        Text(
            "Generate a pairing code in the server's admin UI, then enter it here. " +
                "Leave the pin blank to trust the server on first use.",
            style = MaterialTheme.typography.bodySmall,
        )
        OutlinedTextField(server, { server = it }, label = { Text("Server URL") }, modifier = Modifier.fillMaxWidth())
        OutlinedTextField(otp, { otp = it }, label = { Text("OTP") }, modifier = Modifier.fillMaxWidth())
        OutlinedTextField(name, { name = it }, label = { Text("Device name") }, modifier = Modifier.fillMaxWidth())
        OutlinedTextField(pin, { pin = it }, label = { Text("SPKI pin (optional)") }, modifier = Modifier.fillMaxWidth())
        Button(
            enabled = !busy,
            onClick = {
                busy = true; msg = "pairing…"
                scope.launch {
                    val result = withContext(Dispatchers.IO) {
                        runCatching { claimAndStore(ctx, server.trim(), otp.trim(), name.trim(), pin.trim()) }
                    }
                    busy = false
                    result.onSuccess {
                        SyncService.start(ctx)
                        onPaired()
                    }.onFailure { msg = "failed: ${it.message}" }
                }
            },
        ) { Text(if (busy) "pairing…" else "Pair") }
        if (msg.isNotEmpty()) Text(msg, color = MaterialTheme.colorScheme.error)
    }
}

@Composable
private fun StatusScreen(onUnpair: () -> Unit) {
    val ctx = LocalContext.current
    val settings = remember { Settings(ctx) }
    val dao = remember { HistoryDb.get(ctx).clipDao() }

    val status by SyncState.status.collectAsState()
    val connected by SyncState.connected.collectAsState()
    val lastEvent by SyncState.lastEvent.collectAsState()

    var query by remember { mutableStateOf("") }
    val history by remember(query) {
        if (query.isBlank()) dao.recent() else dao.search(query)
    }.collectAsState(initial = emptyList())
    var canOverlay by remember { mutableStateOf(AndroidSettings.canDrawOverlays(ctx)) }

    Column(
        Modifier
            .fillMaxSize()
            .padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text("CopySync", style = MaterialTheme.typography.headlineMedium)
        Text("server: ${settings.serverName ?: "?"} — ${settings.serverUrl ?: "?"}", style = MaterialTheme.typography.bodySmall)
        Text("status: $status  ${if (connected) "● connected" else "○ offline"}")
        if (lastEvent.isNotEmpty()) Text("last: $lastEvent", style = MaterialTheme.typography.bodySmall)

        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = { SyncService.start(ctx) }) { Text("Start") }
            OutlinedButton(onClick = { SyncService.stop(ctx) }) { Text("Stop") }
            OutlinedButton(onClick = { copyTest(ctx) }) { Text("Copy test") }
        }

        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text("Background capture setup", style = MaterialTheme.typography.titleSmall)
                Text("overlay permission: ${if (canOverlay) "granted" else "NOT granted"}")
                if (!canOverlay) {
                    OutlinedButton(onClick = {
                        ctx.startActivity(
                            Intent(
                                AndroidSettings.ACTION_MANAGE_OVERLAY_PERMISSION,
                                Uri.parse("package:${ctx.packageName}"),
                            ),
                        )
                        canOverlay = AndroidSettings.canDrawOverlays(ctx)
                    }) { Text("Grant overlay") }
                }
                Text("One-time ADB grant (background clipboard read):", style = MaterialTheme.typography.bodySmall)
                SelectionContainer {
                    Text(
                        "adb shell pm grant ${ctx.packageName} android.permission.READ_LOGS\n" +
                            "adb shell appops set ${ctx.packageName} SYSTEM_ALERT_WINDOW allow",
                        fontFamily = FontFamily.Monospace,
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
            }
        }

        OutlinedTextField(
            query, { query = it },
            label = { Text("Search history") },
            modifier = Modifier.fillMaxWidth(),
        )
        LazyColumn(Modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            items(history) { e ->
                val arrow = if (e.direction == "in") "←" else "→"
                Text("$arrow ${e.text.replace("\n", " ").take(90)}", style = MaterialTheme.typography.bodyMedium)
            }
        }

        TextButton(onClick = {
            SyncService.stop(ctx)
            Settings(ctx).clear()
            Secrets(ctx).clear()
            onUnpair()
        }) { Text("Unpair") }
    }
}

private fun copyTest(ctx: Context) {
    val cm = ctx.getSystemService(ClipboardManager::class.java)
    cm.setPrimaryClip(ClipData.newPlainText("CopySync", "test ${System.currentTimeMillis()}"))
}
