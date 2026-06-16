package com.copysync.android.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.copysync.android.sync.BlockedClip
import com.copysync.android.sync.SyncState
import kotlinx.coroutines.delay

private val Pink = Color(0xFFEC4899)

private fun reasonKo(label: String): String = when (label) {
    "password-like" -> "비밀번호로 추정"
    "payment card" -> "카드번호로 추정"
    "private key" -> "개인 키"
    "OTP secret" -> "OTP/2FA 비밀"
    "custom pattern" -> "사용자 패턴"
    else -> "민감 정보"
}

/**
 * Pink translucent glass toast shown (and auto-dismissed) when the privacy filter
 * blocks a clip from syncing. Overlays the whole app; `last` keeps the content
 * around for the exit animation after the state flow is cleared.
 */
@Composable
fun BlockedToastOverlay() {
    val toast by SyncState.blockedToast.collectAsState()
    val last = remember { mutableStateOf<BlockedClip?>(null) }
    LaunchedEffect(toast) {
        if (toast != null) {
            last.value = toast
            delay(4500)
            SyncState.blockedToast.value = null
        }
    }
    Box(
        Modifier.fillMaxSize().padding(bottom = 124.dp, start = 16.dp, end = 16.dp),
        contentAlignment = Alignment.BottomCenter,
    ) {
        AnimatedVisibility(
            visible = toast != null,
            enter = fadeIn() + slideInVertically { it / 2 },
            exit = fadeOut() + slideOutVertically { it / 2 },
        ) {
            last.value?.let { t ->
                Row(
                    Modifier.fillMaxWidth()
                        .clip(RoundedCornerShape(16.dp))
                        .background(Pink.copy(alpha = 0.22f))
                        .border(1.dp, Pink.copy(alpha = 0.55f), RoundedCornerShape(16.dp))
                        .padding(14.dp),
                    verticalAlignment = Alignment.Top,
                ) {
                    Text("🔒", fontSize = 18.sp)
                    Spacer(Modifier.width(10.dp))
                    Column(Modifier.weight(1f)) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text("동기화 차단됨", fontWeight = FontWeight.Bold, fontSize = 14.sp)
                            Spacer(Modifier.width(6.dp))
                            Text(
                                reasonKo(t.reason),
                                color = Color.White,
                                fontSize = 11.sp,
                                fontWeight = FontWeight.Bold,
                                modifier = Modifier
                                    .clip(RoundedCornerShape(999.dp))
                                    .background(Pink.copy(alpha = 0.92f))
                                    .padding(horizontal = 8.dp, vertical = 1.dp),
                            )
                        }
                        Spacer(Modifier.height(3.dp))
                        Text(
                            t.preview,
                            fontSize = 12.sp,
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis,
                            color = LocalContentColor.current.copy(alpha = 0.72f),
                        )
                    }
                }
            }
        }
    }
}
