package com.copysync.android.ui

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.BiasAlignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.blur
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.copysync.android.data.Settings
import kotlinx.coroutines.flow.MutableStateFlow
import java.io.File
import java.io.FileOutputStream

/**
 * Theme-aware semantic colors that Material 3 has no slot for (see DESIGN.md).
 * "Warm Trust" success green: #34D399 (dark) / #15803D (light); warn amber:
 * #FBBF24 (dark) / #9A6406 (light); danger uses [androidx.compose.material3.ColorScheme.error].
 * These are kept hue-separated from the teal `primary` accent on purpose.
 */
/** True when the effective theme is dark, honoring the [ThemeState.mode] override. */
@Composable
fun isDarkTheme(): Boolean {
    val mode by ThemeState.mode.collectAsState()
    return when (mode) {
        "light" -> false
        "dark" -> true
        else -> isSystemInDarkTheme()
    }
}

@Composable
fun successColor(): Color =
    if (isDarkTheme()) Color(0xFF34D399) else Color(0xFF15803D)

@Composable
fun warnColor(): Color =
    if (isDarkTheme()) Color(0xFFFBBF24) else Color(0xFF9A6406)

/** Reactive theme state (persisted via [Settings]). Mirrors the desktop theme. */
object ThemeState {
    val mode = MutableStateFlow("system") // dark | light | system
    val bgPath = MutableStateFlow("")
    val brightness = MutableStateFlow(1f)
    val blur = MutableStateFlow(0f)
    val zoom = MutableStateFlow(1f)
    val biasX = MutableStateFlow(0f) // -1..1 (BiasAlignment)
    val biasY = MutableStateFlow(0f)
    val cardOpacity = MutableStateFlow(1f)

    fun load(ctx: Context) {
        val s = Settings(ctx)
        mode.value = s.themeMode
        bgPath.value = s.bgImagePath
        brightness.value = s.bgBrightness
        blur.value = s.bgBlur
        zoom.value = s.bgZoom
        biasX.value = s.bgX
        biasY.value = s.bgY
        cardOpacity.value = s.cardOpacity
    }
}

/** App theme: dark/light/system color scheme + an optional background image with
 *  brightness/blur/crop, plus translucent surfaces so cards become frosted glass. */
@Composable
fun CopySyncTheme(content: @Composable () -> Unit) {
    val mode by ThemeState.mode.collectAsState()
    val bgPath by ThemeState.bgPath.collectAsState()
    val cardOp by ThemeState.cardOpacity.collectAsState()
    val dark = when (mode) {
        "light" -> false
        "dark" -> true
        else -> isSystemInDarkTheme()
    }
    // "Warm Trust" design system (see clients/desktop/DESIGN.md): warm-neutral
    // tiered surfaces + a single teal action accent, shared with the desktop GUI.
    var scheme = if (dark) darkColorScheme(
        primary = Color(0xFF0E8C84), onPrimary = Color.White,
        secondary = Color(0xFF34D399), tertiary = Color(0xFFA78BFA),
        background = Color(0xFF1A1816), onBackground = Color(0xFFEDE8DF),
        surface = Color(0xFF222019), onSurface = Color(0xFFEDE8DF),
        surfaceVariant = Color(0xFF2B2823), onSurfaceVariant = Color(0xFFA39C8E),
        surfaceContainerLowest = Color(0xFF1A1816), surfaceContainerLow = Color(0xFF222019),
        surfaceContainer = Color(0xFF222019), surfaceContainerHigh = Color(0xFF2B2823),
        surfaceContainerHighest = Color(0xFF332F29),
        outline = Color(0xFF3A362F), outlineVariant = Color(0xFF3A362F),
        error = Color(0xFFF87171),
    ) else lightColorScheme(
        primary = Color(0xFF0B7C72), onPrimary = Color.White,
        secondary = Color(0xFF15803D), tertiary = Color(0xFF7E22CE),
        background = Color(0xFFF6F3EC), onBackground = Color(0xFF23201B),
        surface = Color(0xFFFFFFFF), onSurface = Color(0xFF23201B),
        surfaceVariant = Color(0xFFEFEBE1), onSurfaceVariant = Color(0xFF6E685C),
        surfaceContainerLowest = Color(0xFFFFFFFF), surfaceContainerLow = Color(0xFFF6F3EC),
        surfaceContainer = Color(0xFFEFEBE1), surfaceContainerHigh = Color(0xFFEAE5DA),
        surfaceContainerHighest = Color(0xFFE3DDD0),
        outline = Color(0xFFDAD3C5), outlineVariant = Color(0xFFDAD3C5),
        error = Color(0xFFC81E1E),
    )
    if (bgPath.isNotEmpty() && cardOp < 1f) {
        fun Color.a() = copy(alpha = cardOp)
        scheme = scheme.copy(
            surface = scheme.surface.a(),
            surfaceVariant = scheme.surfaceVariant.a(),
            surfaceContainer = scheme.surfaceContainer.a(),
            surfaceContainerLow = scheme.surfaceContainerLow.a(),
            surfaceContainerLowest = scheme.surfaceContainerLowest.a(),
            surfaceContainerHigh = scheme.surfaceContainerHigh.a(),
            surfaceContainerHighest = scheme.surfaceContainerHighest.a(),
        )
    }
    MaterialTheme(colorScheme = scheme) {
        androidx.compose.foundation.layout.Box(Modifier.fillMaxSize().background(MaterialTheme.colorScheme.background)) {
            if (bgPath.isNotEmpty()) ThemeBackground(dark)
            Surface(color = Color.Transparent, contentColor = MaterialTheme.colorScheme.onBackground, modifier = Modifier.fillMaxSize()) {
                content()
            }
        }
    }
}

@Composable
private fun ThemeBackground(dark: Boolean) {
    val path by ThemeState.bgPath.collectAsState()
    val bright by ThemeState.brightness.collectAsState()
    val blurR by ThemeState.blur.collectAsState()
    val zoom by ThemeState.zoom.collectAsState()
    val bx by ThemeState.biasX.collectAsState()
    val by by ThemeState.biasY.collectAsState()
    val bmp = remember(path) {
        runCatching { BitmapFactory.decodeFile(path)?.asImageBitmap() }.getOrNull()
    } ?: return
    Image(
        bitmap = bmp, contentDescription = null,
        modifier = Modifier.fillMaxSize().blur(blurR.dp).graphicsLayer { scaleX = zoom; scaleY = zoom },
        contentScale = ContentScale.Crop,
        alignment = BiasAlignment(bx.coerceIn(-1f, 1f), by.coerceIn(-1f, 1f)),
    )
    // Readability scrim: with a wallpaper set, always wash it toward the theme
    // (darken in dark mode / lighten in light mode) so translucent surfaces keep
    // enough contrast even over a bright photo — regardless of the brightness slider.
    val dim = maxOf(if (dark) 0.45f else 0f, 1f - bright).coerceIn(0f, 1f)
    if (dim > 0f) androidx.compose.foundation.layout.Box(Modifier.fillMaxSize().background(Color.Black.copy(alpha = dim)))
    val lighten = maxOf(if (!dark) 0.40f else 0f, (bright - 1f) * 0.6f).coerceIn(0f, 0.6f)
    if (lighten > 0f) androidx.compose.foundation.layout.Box(Modifier.fillMaxSize().background(Color.White.copy(alpha = lighten)))
}

/** The "화면 / 테마" card shown in the Settings tab. */
@Composable
fun ThemeSettingsCard() {
    val ctx = LocalContext.current
    val settings = remember { Settings(ctx) }
    val mode by ThemeState.mode.collectAsState()
    val bgPath by ThemeState.bgPath.collectAsState()
    val bright by ThemeState.brightness.collectAsState()
    val blurR by ThemeState.blur.collectAsState()
    val zoom by ThemeState.zoom.collectAsState()
    val cardOp by ThemeState.cardOpacity.collectAsState()
    val bx by ThemeState.biasX.collectAsState()
    val by by ThemeState.biasY.collectAsState()

    val picker = rememberLauncherForActivityResult(ActivityResultContracts.PickVisualMedia()) { uri ->
        if (uri != null) saveBgImage(ctx, uri)?.let { p ->
            ThemeState.bgPath.value = p; settings.bgImagePath = p
            ThemeState.biasX.value = 0f; ThemeState.biasY.value = 0f; settings.bgX = 0f; settings.bgY = 0f
        }
    }

    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text("화면 / 테마", style = MaterialTheme.typography.titleSmall)
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                listOf("dark" to "다크", "light" to "라이트", "system" to "시스템").forEach { (m, label) ->
                    val click = { ThemeState.mode.value = m; settings.themeMode = m }
                    if (mode == m) Button(onClick = click, modifier = Modifier.weight(1f)) { Text(label) }
                    else OutlinedButton(onClick = click, modifier = Modifier.weight(1f)) { Text(label) }
                }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = { picker.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly)) }) { Text("배경 이미지") }
                if (bgPath.isNotEmpty()) OutlinedButton(onClick = { ThemeState.bgPath.value = ""; settings.bgImagePath = "" }) { Text("제거") }
            }
            if (bgPath.isNotEmpty()) {
                CropPreview(bgPath, zoom, bx, by) { nx, ny ->
                    ThemeState.biasX.value = nx; ThemeState.biasY.value = ny; settings.bgX = nx; settings.bgY = ny
                }
                ThemeSlider("확대", zoom, 1f, 3f) { ThemeState.zoom.value = it; settings.bgZoom = it }
                ThemeSlider("밝기", bright, 0.3f, 1.3f) { ThemeState.brightness.value = it; settings.bgBrightness = it }
                ThemeSlider("흐림(블러)", blurR, 0f, 24f) { ThemeState.blur.value = it; settings.bgBlur = it }
                ThemeSlider("박스 투명도", cardOp, 0.3f, 1f) { ThemeState.cardOpacity.value = it; settings.cardOpacity = it }
                Text("미리보기를 드래그해 위치를 조정하세요.", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
}

@Composable
private fun ThemeSlider(label: String, value: Float, min: Float, max: Float, onChange: (Float) -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(label, modifier = Modifier.width(86.dp), style = MaterialTheme.typography.bodySmall)
        Slider(value = value, onValueChange = onChange, valueRange = min..max, modifier = Modifier.weight(1f))
        Text(String.format("%.1f", value), modifier = Modifier.width(40.dp), style = MaterialTheme.typography.bodySmall)
    }
}

@Composable
private fun CropPreview(path: String, zoom: Float, bx: Float, by: Float, onPan: (Float, Float) -> Unit) {
    val bmp = remember(path) { runCatching { BitmapFactory.decodeFile(path)?.asImageBitmap() }.getOrNull() } ?: return
    androidx.compose.foundation.layout.Box(
        Modifier.fillMaxWidth().height(130.dp).clip(RoundedCornerShape(10.dp))
            .pointerInput(Unit) {
                detectDragGestures { _, drag ->
                    val nx = (ThemeState.biasX.value - drag.x / size.width * 2f).coerceIn(-1f, 1f)
                    val ny = (ThemeState.biasY.value - drag.y / size.height * 2f).coerceIn(-1f, 1f)
                    onPan(nx, ny)
                }
            },
    ) {
        Image(
            bitmap = bmp, contentDescription = null,
            modifier = Modifier.fillMaxSize().graphicsLayer { scaleX = zoom; scaleY = zoom },
            contentScale = ContentScale.Crop, alignment = BiasAlignment(bx, by),
        )
    }
}

/** Copy a picked image into app storage (downscaled JPEG) and return its path. */
private fun saveBgImage(ctx: Context, uri: Uri): String? = runCatching {
    val raw = ctx.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it) } ?: return null
    val max = 1600
    val sc = minOf(1f, max.toFloat() / maxOf(raw.width, raw.height))
    val scaled = if (sc < 1f) Bitmap.createScaledBitmap(raw, (raw.width * sc).toInt(), (raw.height * sc).toInt(), true) else raw
    val f = File(ctx.filesDir, "themebg.jpg")
    FileOutputStream(f).use { scaled.compress(Bitmap.CompressFormat.JPEG, 85, it) }
    f.absolutePath
}.getOrNull()
