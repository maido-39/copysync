package com.copysync.android.ui

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.BitmapFactory
import android.net.Uri
import android.provider.Settings as AndroidSettings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Checkbox
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.copysync.android.data.ClipEntity
import com.copysync.android.data.Downloads
import com.copysync.android.data.HistoryDb
import com.copysync.android.data.Secrets
import com.copysync.android.data.Settings
import com.copysync.android.net.MdnsDiscovery
import com.copysync.android.net.claimAndStore
import com.copysync.android.net.parsePairQr
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import com.copysync.android.sync.Notifications
import com.copysync.android.sync.DebugLog
import com.copysync.android.sync.SyncService
import com.copysync.android.sync.SyncState
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File

@Composable
fun AppRoot() {
    val ctx = LocalContext.current
    var paired by remember { mutableStateOf(Settings(ctx).isPaired) }
    if (paired) MainScaffold(onUnpair = { paired = false })
    else PairingScreen(onPaired = { paired = true })
}

// ---------------------------------------------------------------- Pairing

@Composable
private fun PairingScreen(onPaired: () -> Unit) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    var server by remember { mutableStateOf("https://192.168.20.177:8443") }
    var otp by remember { mutableStateOf("") }
    var name by remember { mutableStateOf("android") }
    var pin by remember { mutableStateOf("") }
    var msg by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var scanning by remember { mutableStateOf(false) }
    val found = remember { mutableStateListOf<Pair<String, String>>() }
    val discovery = remember { MdnsDiscovery(ctx) }
    DisposableEffect(Unit) { onDispose { discovery.stop() } }

    fun doPair() {
        busy = true; msg = "페어링 중…"
        scope.launch {
            val result = withContext(Dispatchers.IO) {
                runCatching { claimAndStore(ctx, server.trim(), otp.trim(), name.trim(), pin.trim()) }
            }
            busy = false
            result.onSuccess { SyncService.start(ctx); onPaired() }
                .onFailure { msg = "실패: ${it.message}" }
        }
    }
    // In-app camera QR scanner: decode the admin's pairing QR, fill the fields, pair.
    val scanLauncher = rememberLauncherForActivityResult(ScanContract()) { res ->
        val contents = res.contents
        if (contents != null) {
            runCatching {
                val qr = parsePairQr(contents)
                require(qr.host.isNotBlank() && qr.otp.isNotBlank()) { "host/otp 누락" }
                server = "https://${qr.host}:${qr.port.ifBlank { "8443" }}"
                otp = qr.otp
                pin = qr.spkiPin
            }.onSuccess { msg = "QR 인식됨 — 페어링 중…"; doPair() }
                .onFailure { msg = "QR 형식 오류: ${it.message}" }
        }
    }

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Text("기기 페어링", style = MaterialTheme.typography.headlineSmall)
        Text(
            "서버 관리자 화면에서 OTP를 생성해 입력하세요. 핀을 비우면 최초 접속 시 자동 신뢰합니다.",
            style = MaterialTheme.typography.bodySmall,
        )
        OutlinedTextField(server, { server = it }, label = { Text("서버 주소") }, singleLine = true, modifier = Modifier.fillMaxWidth())
        OutlinedButton(enabled = !scanning, modifier = Modifier.fillMaxWidth(), onClick = {
            found.clear(); scanning = true
            discovery.start { n, url -> if (found.none { it.second == url }) found.add(n to url) }
            scope.launch { delay(5000); discovery.stop(); scanning = false }
        }) { Text(if (scanning) "검색 중…" else "같은 네트워크에서 서버 검색") }
        found.forEach { (n, url) ->
            TextButton(onClick = { server = url }, modifier = Modifier.fillMaxWidth()) { Text("$n — $url") }
        }
        Button(enabled = !busy, modifier = Modifier.fillMaxWidth(), onClick = {
            scanLauncher.launch(
                ScanOptions().apply {
                    setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                    setPrompt("관리자 화면의 페어링 QR을 비추세요")
                    setBeepEnabled(false)
                    setOrientationLocked(false)
                },
            )
        }) { Text("📷 QR 코드 스캔") }
        Text("— 또는 직접 입력 —", style = MaterialTheme.typography.bodySmall)
        OutlinedTextField(otp, { otp = it }, label = { Text("OTP") }, singleLine = true, modifier = Modifier.fillMaxWidth())
        OutlinedTextField(name, { name = it }, label = { Text("기기 이름") }, singleLine = true, modifier = Modifier.fillMaxWidth())
        OutlinedTextField(pin, { pin = it }, label = { Text("SPKI 핀 (선택)") }, singleLine = true, modifier = Modifier.fillMaxWidth())
        Button(enabled = !busy, onClick = { doPair() }) { Text(if (busy) "페어링 중…" else "페어링") }
        if (msg.isNotEmpty()) Text(msg, color = MaterialTheme.colorScheme.error)
    }
}

// ---------------------------------------------------------------- Tabs shell

@Composable
private fun MainScaffold(onUnpair: () -> Unit) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val dao = remember { HistoryDb.get(ctx).clipDao() }
    var tab by remember { mutableIntStateOf(0) }

    // Save-location picker (works from any tab; triggered by the 받기 button or a
    // notification tap that routes through PendingDownload).
    val pending by PendingDownload.req.collectAsState()
    var current by remember { mutableStateOf<DownloadReq?>(null) }
    val savePicker = rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("*/*")) { uri ->
        val req = current
        current = null
        PendingDownload.req.value = null
        if (uri != null && req != null) {
            scope.launch {
                withContext(Dispatchers.IO) { runCatching { Downloads.fetchToUri(ctx, req.blobId, uri, req.encrypted) } }
                    .onSuccess {
                        if (req.rowid >= 0) dao.setLocalPath(req.rowid, uri.toString())
                        Notifications.notifyInfo(ctx, "CopySync", "다운로드 완료: ${req.name}")
                    }
                    .onFailure { Notifications.notifyInfo(ctx, "CopySync", "다운로드 실패: ${it.message}") }
            }
        }
    }
    LaunchedEffect(pending) {
        val p = pending ?: return@LaunchedEffect
        current = p
        savePicker.launch(p.name)
    }

    val tabs = listOf("연결" to "🔗", "기록" to "📋", "설정" to "⚙️", "디버깅" to "🐞")
    Scaffold(bottomBar = {
        NavigationBar {
            tabs.forEachIndexed { i, (label, icon) ->
                NavigationBarItem(
                    selected = tab == i,
                    onClick = { tab = i },
                    icon = { Text(icon, fontSize = 18.sp) },
                    label = { Text(label) },
                )
            }
        }
    }) { pad ->
        Box(Modifier.padding(pad).fillMaxSize()) {
            when (tab) {
                0 -> ConnectionTab(onUnpair)
                1 -> HistoryTab()
                2 -> SettingsTab()
                else -> DebugTab()
            }
        }
    }
}

// ---------------------------------------------------------------- 연결

@Composable
private fun ConnectionTab(onUnpair: () -> Unit) {
    val ctx = LocalContext.current
    val settings = remember { Settings(ctx) }
    val status by SyncState.status.collectAsState()
    val connected by SyncState.connected.collectAsState()
    val lastEvent by SyncState.lastEvent.collectAsState()

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("CopySync", style = MaterialTheme.typography.headlineMedium)
        Card(
            Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(
                containerColor = if (connected) Color(0xFF16A34A) else Color(0xFF9E9E9E),
                contentColor = Color.White,
            ),
        ) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text(if (connected) "● 연결됨" else "○ 오프라인", style = MaterialTheme.typography.titleMedium)
                Text(status, style = MaterialTheme.typography.bodyMedium)
                if (lastEvent.isNotEmpty()) Text("최근: $lastEvent", style = MaterialTheme.typography.bodySmall)
            }
        }
        InfoRow("서버", "${settings.serverName ?: "?"}")
        InfoRow("주소", "${settings.serverUrl ?: "?"}")
        InfoRow("기기 이름", "${settings.deviceName ?: "?"}")
        PoolCard()
        RoutingCard()
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = { SyncService.start(ctx) }) { Text("시작") }
            OutlinedButton(onClick = { SyncService.stop(ctx) }) { Text("중지") }
        }
        Spacer(Modifier.size(8.dp))
        TextButton(onClick = {
            SyncService.stop(ctx)
            Settings(ctx).clear()
            Secrets(ctx).clear()
            onUnpair()
        }) { Text("페어링 해제", color = MaterialTheme.colorScheme.error) }
    }
}

/** Share pool: clips only sync among devices in the same pool. */
@Composable
private fun PoolCard() {
    val ctx = LocalContext.current
    val pools by SyncState.pools.collectAsState()
    val current by SyncState.currentPool.collectAsState()
    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Text("공유 풀", style = MaterialTheme.typography.titleSmall)
            Text(
                "같은 풀의 기기끼리만 동기화됩니다 · 현재: $current",
                style = MaterialTheme.typography.bodySmall, color = Color.Gray,
            )
            val list = if (pools.isEmpty()) listOf("default") else pools
            Row(
                Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                list.forEach { p ->
                    if (p == current) Button(onClick = {}) { Text(p) }
                    else OutlinedButton(onClick = { SyncService.setPool(ctx, p) }) { Text(p) }
                }
            }
        }
    }
}

/** Routing: pick specific target devices, or leave all unchecked to broadcast. */
@Composable
private fun RoutingCard() {
    val roster by SyncState.roster.collectAsState()
    val targets by SyncState.targets.collectAsState()
    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text("전송 대상", style = MaterialTheme.typography.titleSmall)
            if (roster.isEmpty()) {
                Text("연결된 다른 기기가 없습니다 · 전체 전송", style = MaterialTheme.typography.bodySmall, color = Color.Gray)
            } else {
                Text(
                    if (targets.isEmpty()) "전체 기기로 브로드캐스트" else "${targets.size}개 기기로만 전송",
                    style = MaterialTheme.typography.bodySmall, color = Color.Gray,
                )
                roster.forEach { d ->
                    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                        Checkbox(
                            checked = targets.contains(d.id),
                            onCheckedChange = { on ->
                                val s = SyncState.targets.value.toMutableSet()
                                if (on) s.add(d.id) else s.remove(d.id)
                                SyncState.targets.value = s
                            },
                        )
                        Text(
                            "${if (d.online) "● " else "○ "}${d.name.ifEmpty { d.id.take(8) }}",
                            Modifier.weight(1f),
                            style = MaterialTheme.typography.bodyMedium,
                        )
                    }
                }
                if (targets.isNotEmpty()) {
                    TextButton(onClick = { SyncState.targets.value = emptySet() }) { Text("전체로 초기화") }
                }
            }
        }
    }
}

@Composable
private fun InfoRow(label: String, value: String) {
    Row(Modifier.fillMaxWidth()) {
        Text(label, modifier = Modifier.width(80.dp), style = MaterialTheme.typography.bodySmall, color = Color.Gray)
        Text(value, style = MaterialTheme.typography.bodyMedium)
    }
}

// ---------------------------------------------------------------- 기록

@Composable
private fun HistoryTab() {
    val ctx = LocalContext.current
    val dao = remember { HistoryDb.get(ctx).clipDao() }
    var query by remember { mutableStateOf("") }
    val history by remember(query) {
        if (query.isBlank()) dao.recent() else dao.search(query)
    }.collectAsState(initial = emptyList())

    Column(Modifier.fillMaxSize().padding(12.dp)) {
        OutlinedTextField(
            query, { query = it },
            label = { Text("기록 검색") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.size(8.dp))
        if (history.isEmpty()) {
            Text("기록이 없습니다.", style = MaterialTheme.typography.bodyMedium, color = Color.Gray)
        } else {
            LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                items(history, key = { it.rowid }) { e -> HistoryCard(e) }
            }
        }
    }
}

@Composable
private fun HistoryCard(e: ClipEntity) {
    val ctx = LocalContext.current
    Card(
        Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = categoryColor(e)),
    ) {
        Row(Modifier.padding(12.dp), verticalAlignment = Alignment.CenterVertically) {
            Box(Modifier.size(46.dp), contentAlignment = Alignment.Center) {
                val thumb = if (e.kind == "image" || e.mime.startsWith("video/")) rememberThumb(ctx, e) else null
                if (thumb != null) {
                    Image(
                        thumb, contentDescription = null,
                        modifier = Modifier.size(46.dp).clip(RoundedCornerShape(8.dp)),
                        contentScale = ContentScale.Crop,
                    )
                } else {
                    Text(iconFor(e), fontSize = 24.sp)
                }
            }
            Spacer(Modifier.size(12.dp))
            Column(Modifier.weight(1f)) {
                val dir = if (e.direction == "in") "←" else "→"
                val title = if (e.kind == "text") e.text.replace("\n", " ").trim() else e.name.ifEmpty { e.text }
                Text("$dir $title", maxLines = 2, style = MaterialTheme.typography.bodyMedium)
                if (e.kind == "file" && e.mime.startsWith("text/") && e.localPath != null) {
                    val snip = rememberTextSnippet(ctx, e.localPath)
                    if (!snip.isNullOrEmpty()) Text(
                        snip.trim(), maxLines = 3,
                        style = MaterialTheme.typography.bodySmall, color = Color(0xFF455A64),
                    )
                }
                val meta = buildString {
                    if (e.kind != "text") {
                        append(e.mime.ifEmpty { e.kind })
                        if (e.sizeBytes > 0) append(" · ${formatBytes(e.sizeBytes)}")
                        append(" · ")
                    }
                    append(relTime(e.ts))
                }
                Text(meta, style = MaterialTheme.typography.bodySmall, color = Color(0xFF607D8B))
            }
            val canDownload = e.direction == "in" && e.blobId.isNotEmpty() &&
                (e.kind == "file" || e.kind == "image") && e.localPath == null
            when {
                canDownload -> TextButton(onClick = {
                    PendingDownload.req.value = DownloadReq(e.blobId, e.name.ifEmpty { "file" }, e.mime, e.rowid, e.enc)
                }) { Text("⬇ 받기") }
                e.localPath != null -> Text("✓ 저장됨", style = MaterialTheme.typography.bodySmall, color = Color(0xFF16A34A))
            }
        }
    }
}

// ---------------------------------------------------------------- 설정

@Composable
private fun SettingsTab() {
    val ctx = LocalContext.current
    val settings = remember { Settings(ctx) }
    var canOverlay by remember { mutableStateOf(AndroidSettings.canDrawOverlays(ctx)) }
    var e2ePass by remember { mutableStateOf(Secrets(ctx).e2ePass ?: "") }
    var sensitive by remember { mutableStateOf(settings.sensitiveMark) }
    var autoClear by remember { mutableStateOf(settings.autoClearSeconds) }

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("설정", style = MaterialTheme.typography.headlineSmall)
        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("E2E 암호화 (선택)", style = MaterialTheme.typography.titleSmall)
                Text(
                    "모든 기기에 같은 패스프레이즈를 입력하면 서버가 클립 내용을 볼 수 없습니다(영지식). 비우면 끔.",
                    style = MaterialTheme.typography.bodySmall, color = Color.Gray,
                )
                OutlinedTextField(e2ePass, { e2ePass = it }, label = { Text("패스프레이즈") }, singleLine = true, modifier = Modifier.fillMaxWidth())
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    Button(onClick = {
                        Secrets(ctx).e2ePass = e2ePass.ifBlank { null }
                        SyncService.stop(ctx); SyncService.start(ctx)
                    }) { Text("적용 후 재시작") }
                    Text(if (e2ePass.isBlank()) "현재: 꺼짐" else "현재: 켜짐", style = MaterialTheme.typography.bodySmall)
                }
            }
        }
        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("백그라운드 캡처 권한", style = MaterialTheme.typography.titleSmall)
                Text("오버레이: ${if (canOverlay) "허용됨 ✓" else "필요함"}")
                if (!canOverlay) {
                    OutlinedButton(onClick = {
                        ctx.startActivity(
                            Intent(
                                AndroidSettings.ACTION_MANAGE_OVERLAY_PERMISSION,
                                Uri.parse("package:${ctx.packageName}"),
                            ),
                        )
                        canOverlay = AndroidSettings.canDrawOverlays(ctx)
                    }) { Text("오버레이 권한 열기") }
                }
                Text(
                    "백그라운드에서 다른 앱의 복사를 잡으려면 READ_LOGS도 필요합니다 (디버깅 탭의 ADB/Shizuku 명령 참고).",
                    style = MaterialTheme.typography.bodySmall, color = Color.Gray,
                )
            }
        }
        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("보안", style = MaterialTheme.typography.titleSmall)
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
                    Text("받은 항목 민감 표시", modifier = Modifier.weight(1f), style = MaterialTheme.typography.bodyMedium)
                    Switch(checked = sensitive, onCheckedChange = { sensitive = it; settings.sensitiveMark = it })
                }
                Text("자동 삭제 (받은 뒤 클립보드 비우기)", style = MaterialTheme.typography.bodySmall, color = Color.Gray)
                Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    listOf(0 to "끔", 30 to "30초", 60 to "1분", 300 to "5분").forEach { (s, label) ->
                        if (autoClear == s) {
                            Button(onClick = { autoClear = s; settings.autoClearSeconds = s }) { Text(label) }
                        } else {
                            OutlinedButton(onClick = { autoClear = s; settings.autoClearSeconds = s }) { Text(label) }
                        }
                    }
                }
            }
        }
        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text("서버", style = MaterialTheme.typography.titleSmall)
                InfoRow("이름", settings.serverName ?: "?")
                InfoRow("주소", settings.serverUrl ?: "?")
                InfoRow("E2E", if (settings.e2e) "켜짐" else "꺼짐")
                val t = settings.onDemandThreshold
                InfoRow("온디맨드", if (t > 0) "${formatBytes(t)} 초과 시" else "—")
            }
        }
        Text(
            "큰 파일은 클립보드로 못 보냅니다 — 다른 앱에서 공유 → CopySync 로 보내세요.",
            style = MaterialTheme.typography.bodySmall, color = Color.Gray,
        )
    }
}

// ---------------------------------------------------------------- 디버깅

@Composable
private fun DebugTab() {
    val ctx = LocalContext.current
    val status by SyncState.status.collectAsState()
    val connected by SyncState.connected.collectAsState()
    val lastEvent by SyncState.lastEvent.collectAsState()
    val pkg = ctx.packageName

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("디버깅", style = MaterialTheme.typography.headlineSmall)
        InfoRow("연결", if (connected) "connected" else "offline")
        InfoRow("상태", status)
        InfoRow("최근", lastEvent.ifEmpty { "—" })
        OutlinedButton(onClick = {
            ctx.getSystemService(ClipboardManager::class.java)
                .setPrimaryClip(ClipData.newPlainText("CopySync", "test ${System.currentTimeMillis()}"))
        }) { Text("클립보드 테스트 복사") }

        val dbgOn by DebugLog.enabled.collectAsState()
        val dbgLines by DebugLog.lines.collectAsState()
        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                    Text("이벤트 기록", Modifier.weight(1f), style = MaterialTheme.typography.titleSmall)
                    Switch(checked = dbgOn, onCheckedChange = { DebugLog.enabled.value = it })
                }
                Text(
                    if (dbgOn) "모든 동기화·캡처 이벤트 기록 중 (${dbgLines.size}줄). 문제를 재현한 뒤 공유하세요."
                    else "켜면 모든 이벤트를 기록합니다 (개발/디버깅용).",
                    style = MaterialTheme.typography.bodySmall, color = Color.Gray,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Button(enabled = dbgLines.isNotEmpty(), onClick = {
                        val send = Intent(Intent.ACTION_SEND).apply {
                            type = "text/plain"
                            putExtra(Intent.EXTRA_TEXT, dbgLines.joinToString("\n"))
                        }
                        ctx.startActivity(
                            Intent.createChooser(send, "CopySync 로그 공유").addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                        )
                    }) { Text("로그 공유") }
                    OutlinedButton(onClick = { DebugLog.clear() }) { Text("지우기") }
                }
                if (dbgLines.isNotEmpty()) {
                    SelectionContainer {
                        Text(
                            dbgLines.takeLast(60).joinToString("\n"),
                            fontFamily = FontFamily.Monospace, fontSize = 10.sp,
                            style = MaterialTheme.typography.bodySmall,
                        )
                    }
                }
            }
        }
        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text("1회 권한 부여 (ADB 또는 Shizuku rish)", style = MaterialTheme.typography.titleSmall)
                SelectionContainer {
                    Text(
                        "pm grant $pkg android.permission.READ_LOGS\n" +
                            "appops set $pkg SYSTEM_ALERT_WINDOW allow",
                        fontFamily = FontFamily.Monospace,
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
                Text(
                    "부여 후 앱을 완전히 종료했다가 다시 여세요 (logcat 권한 적용).",
                    style = MaterialTheme.typography.bodySmall, color = Color.Gray,
                )
            }
        }
    }
}

// ---------------------------------------------------------------- helpers

private fun categoryColor(e: ClipEntity): Color {
    val m = e.mime.lowercase()
    val ext = e.name.substringAfterLast('.', "").lowercase()
    return when {
        e.kind == "text" -> Color(0xFFF1F3F4)
        m.startsWith("image/") -> Color(0xFFE3F2FD)
        m.startsWith("video/") -> Color(0xFFF3E5F5)
        m.startsWith("audio/") -> Color(0xFFE0F2F1)
        m.contains("pdf") || ext in setOf("doc", "docx", "txt", "md", "rtf", "odt", "hwp") -> Color(0xFFFFF3E0)
        ext in setOf("zip", "rar", "7z", "tar", "gz", "apk") -> Color(0xFFFFF8E1)
        else -> Color(0xFFECEFF1)
    }
}

private fun iconFor(e: ClipEntity): String {
    val m = e.mime.lowercase()
    val ext = e.name.substringAfterLast('.', "").lowercase()
    return when {
        e.kind == "text" -> "🔤"
        m.startsWith("image/") -> "🖼️"
        m.startsWith("video/") -> "🎞️"
        m.startsWith("audio/") -> "🎵"
        m.contains("pdf") -> "📕"
        ext in setOf("zip", "rar", "7z", "tar", "gz", "apk") -> "📦"
        else -> "📄"
    }
}

private fun formatBytes(b: Long): String = when {
    b >= 1024L * 1024 * 1024 -> "%.1f GB".format(b / 1024.0 / 1024 / 1024)
    b >= 1024L * 1024 -> "%.1f MB".format(b / 1024.0 / 1024)
    b >= 1024 -> "%.0f KB".format(b / 1024.0)
    else -> "$b B"
}

private fun relTime(ts: Long): String {
    val d = System.currentTimeMillis() - ts
    return when {
        d < 60_000 -> "방금"
        d < 3_600_000 -> "${d / 60_000}분 전"
        d < 86_400_000 -> "${d / 3_600_000}시간 전"
        else -> "${d / 86_400_000}일 전"
    }
}

@Composable
private fun rememberThumb(ctx: Context, e: ClipEntity): ImageBitmap? {
    val isVideo = e.mime.startsWith("video/")
    val sha = e.blobId.removePrefix("sha256:")
    val thumbPath = if (sha.isNotEmpty()) File(File(ctx.cacheDir, "clip-thumb"), sha).absolutePath else null
    val cachePath = if (sha.isNotEmpty()) File(File(ctx.cacheDir, "clip-src"), sha).absolutePath else null
    val state = produceState<ImageBitmap?>(initialValue = null, key1 = sha, key2 = e.localPath ?: "") {
        value = withContext(Dispatchers.IO) {
            // 1) dedicated preview cache (items we sent + decoded video frames),
            // 2) decrypted on-demand cache (items we received),
            // 3) the downloaded SAF file.
            thumbPath?.let { decodeThumb(it) }
                ?: cachePath?.let { decodeThumb(it) }
                ?: if (isVideo) e.localPath?.let { videoFrameUri(ctx, it) }
                else e.localPath?.let { decodeThumbUri(ctx, it) }
        }
    }
    return state.value
}

private fun decodeThumb(path: String, maxPx: Int = 256): ImageBitmap? = runCatching {
    val f = File(path)
    if (!f.exists()) return null
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    BitmapFactory.decodeFile(path, bounds)
    if (bounds.outWidth <= 0) return null
    var sample = 1
    while (bounds.outWidth / sample > maxPx || bounds.outHeight / sample > maxPx) sample *= 2
    val opts = BitmapFactory.Options().apply { inSampleSize = sample }
    BitmapFactory.decodeFile(path, opts)?.asImageBitmap()
}.getOrNull()

/** Decode a downsampled thumbnail from a content:// URI (downloaded files). */
private fun decodeThumbUri(ctx: Context, uriStr: String, maxPx: Int = 256): ImageBitmap? = runCatching {
    val uri = Uri.parse(uriStr)
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    ctx.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, bounds) }
    if (bounds.outWidth <= 0) return null
    var sample = 1
    while (bounds.outWidth / sample > maxPx || bounds.outHeight / sample > maxPx) sample *= 2
    val opts = BitmapFactory.Options().apply { inSampleSize = sample }
    ctx.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, opts) }?.asImageBitmap()
}.getOrNull()

/** A representative frame from a downloaded video, for history previews. */
private fun videoFrameUri(ctx: Context, uriStr: String): ImageBitmap? = runCatching {
    val mmr = android.media.MediaMetadataRetriever()
    try {
        mmr.setDataSource(ctx, Uri.parse(uriStr))
        mmr.getFrameAtTime(0)?.asImageBitmap()
    } finally {
        mmr.release()
    }
}.getOrNull()

/** First ~400 chars of a downloaded text file, for history previews. */
@Composable
private fun rememberTextSnippet(ctx: Context, uriStr: String): String? {
    val state = produceState<String?>(initialValue = null, key1 = uriStr) {
        value = withContext(Dispatchers.IO) {
            runCatching {
                ctx.contentResolver.openInputStream(Uri.parse(uriStr))?.use { input ->
                    val buf = ByteArray(4096)
                    val n = input.read(buf).coerceAtLeast(0)
                    String(buf, 0, n).take(400)
                }
            }.getOrNull()
        }
    }
    return state.value
}
