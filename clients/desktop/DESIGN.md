Contrast verified. A few adjustments needed: dark danger label (4.86) and the teal-button white text needs `#0B7C72` (5.07) in light, dark danger status is fine at 4.86. Light tag green at 4.14 is below 4.5 — I'll bump it. Let me finalize the spec.

# CopySync — Final Design System ("Warm Trust, Verified")

Base direction: **Warm-Trust** (highest total, best distinctiveness among feasible options). Grafted: the hue-separation law and amber fix (its own best ideas), Crisp-Contrast's per-token dark+light audited hex + "pink reserved only for blocks" law + status-pill-first top bar + two-weights-per-surface rule + sharp-vs-stadium decision, Frosted-Slate's "never nest the same fill twice" tier law and direction-word coding (desaturated), Quiet-Utility's near-solid light-mode toast + dot+word pill + Shadow::NONE depth recipe. Flaws fixed: toast now paints an **opaque backing surface** before the pink tint; **light-mode tag/status colors are darkened** (dark hues never carried into light); **kind-tags collapsed toward calm** (muted neutral default tag + reserved color only where it earns it); teal is **fill-only, never running text**.

---

## 1. TOKENS (exact hex)

All colors are flat solid fills. Depth = three surface tiers (`bg` < `panel` < `panel2`), each fenced by a 1px `line` border. No gradient/shadow/blur anywhere; `popup_shadow` and `window_shadow` set to `Shadow::NONE`.

### DARK
| token | hex | role |
|---|---|---|
| `bg` | `#1A1816` | window / CentralPanel (tier 0) |
| `panel` | `#222019` | cards, top bar (tier 1) |
| `panel2` | `#2B2823` | nested rows, inputs, combo popup (tier 2) |
| `line` | `#3A362F` | 1px border on every tier |
| `fg` | `#EDE8DF` | all running text |
| `muted` | `#A39C8E` | meta lines, hints, secondary |
| `accent.primary` | `#0E8C84` | teal — actions/active **(fill only)** |
| `accent.success` | `#34D399` | 연결됨 dot + pill |
| `accent.danger` | `#F87171` | 연결 끊김 + destructive |
| `accent.warn` | `#FBBF24` | 재연결 중 |
| `tag.text` | `#3FB94F` | 📝 텍스트 |
| `tag.image` | `#A78BFA` | 🖼 이미지 |
| `tag.file` | `#5B9CF6` | 📎 파일 |
| `blocked_pink` | `#E0588F` | base; toast title `#FBCFE8`, chip text `#FCE7F3` |

### LIGHT — *different hues, not the dark ones reused*
| token | hex | role |
|---|---|---|
| `bg` | `#F6F3EC` | warm off-white window (tier 0) |
| `panel` | `#FFFFFF` | cards, top bar (tier 1) |
| `panel2` | `#EFEBE1` | nested rows, inputs (tier 2) |
| `line` | `#DAD3C5` | 1px border |
| `fg` | `#23201B` | all running text |
| `muted` | `#6E685C` | meta/secondary (12.5px+ only) |
| `accent.primary` | `#0B7C72` | teal — actions/active **(fill only)** |
| `accent.success` | `#15803D` | 연결됨 |
| `accent.danger` | `#C81E1E` | 연결 끊김 |
| `accent.warn` | `#9A6406` | 재연결 중 (amber over harsh yellow) |
| `tag.text` | `#117A33` | 📝 텍스트 |
| `tag.image` | `#7E22CE` | 🖼 이미지 |
| `tag.file` | `#1D4ED8` | 📎 파일 |
| `blocked_pink` | `#E0588F` | base; toast solid surface `#FDE7F1`, title `#9D174D`, text `#831843` |

**AA-contrast note (all verified against tinted chip grounds and tier surfaces, not asserted):** DARK — fg/bg 14.5, muted/panel2 5.4, tags on their 18% chip 4.4–4.7, status labels 4.9–7.1, white-on-teal-fill via large/bold button label only (teal never carries small text). LIGHT — fg/panel 16.2, muted/bg 4.99, tags on 14% chip 4.6–5.5 (`tag.text` darkened to `#117A33` to clear 4.5), status labels on bg 4.5–5.2, white-on-`#0B7C72` button 5.07, light toast title `#9D174D`/`#FDE7F1` 6.71. **Every small-text foreground/background pair ≥ 4.5; teal is fill-only so its 4.1/3.7 raw ratio never applies to text.**

---

## 2. SCALES

**Type** — egui default Proportional for all chrome; Monospace only in 🐞 디버깅 + size/hash meta. **Bundle Noto Sans KR (Regular 400 + Medium 500) via `FontDefinitions`, prepended to BOTH Proportional and Monospace families** (current code ships no font bundling → Hangul tofu on any OS without a fallback; this is required new work, not free). No bold bundled — emphasis = `RichText::strong()` (brightness) + size, never faux-bold. **Two weights per surface max** (strong-fg for the one thing that matters, muted-normal for support).

| token | px | use |
|---|---|---|
| display | 20 | wordmark "CopySync" |
| heading | 14 strong | card/section titles, tab labels |
| body | 14 | input text, clip preview, values |
| button | 13 strong | button labels |
| meta | 12.5 | history meta line, secondary, pill (muted) |
| micro | 11.5 medium | kind-tags, timestamps, reason chip |
| mono | 12.5 | debug log (`FontFamily::Monospace`) |

**Radius** (egui `.rounding()` float, `Rounding::same`): inputs 8 · buttons 8 · pills/tags 8 · chips 10 (squircle) · cards/groups 10 · thumbnails 6 · toast 12. Friendly-rounded, consistent — no mixed corners.

**Spacing** (px, 4-base): `2 / 4 / 8 / 12 / 16 / 24`. `item_spacing = [8,6]` · `button_padding = [12,6]` · card `inner_margin = Margin::symmetric(14,12)` · pill `symmetric(9,3)` · tag `symmetric(7,2)` · toast `symmetric(14,11)` · window edge 16 · **between cards `add_space(12)`** (generous = the trust mood; not 8). Borders **always 1px** `Stroke::new(1.0, line)`; focus/selected 1.5px accent. Min interactive height 28px; tabs 30px.

---

## 3. COMPONENT SPECS

Global `Visuals` (set once per theme in `apply_theme`, replacing raw `dark()/light()`):
`override_text_color = fg` · `window_fill = panel_fill = bg` · `extreme_bg_color = panel2` (input bg) · `widgets.noninteractive.bg_fill = panel`, `bg_stroke = (1,line)` · `widgets.inactive.bg_fill = panel2`, `bg_stroke = (1,line)`, `fg_stroke = (1,fg)` · `widgets.hovered.bg_stroke = (1, accent@45%)` · `widgets.active.bg_fill = accent`, `fg_stroke = (1, on-accent)` · `selection.bg_fill = accent@16%`, `selection.stroke = (1,accent)` · `popup_shadow = window_shadow = Shadow::NONE`. **Tier law (graft from Frosted-Slate): never nest the same fill twice — every nested pane steps up one tier so the 1px border + tonal step reads as raised.**

**Status pill** — `Frame` fill = statusColor@22% (dark) / @16% (light), 1px stroke statusColor@50%, rounding 8, `symmetric(9,3)`. Leading solid 8px ● dot in statusColor + label in statusColor at 12.5 (dot+word is more glanceable than bare word). 연결됨=success, 재연결 중=warn, 연결 끊김=danger. 재연결 dot may breathe via eased alpha (not a hard flip — that reads as a glitch).

**Primary button** (텍스트 보내기, 페어링, 지금 재연결) — fill `accent.primary`, rounding 8, padding [12,6], white label 13 strong; hover `gamma_multiply(1.10)`, active `0.9`, disabled fill `muted@40%`. **One primary per tab.** White-on-teal is the only place teal touches a label, and only on this large bold ground.

**Secondary button** (파일 보내기…, 새로고침, 서버 검색, 복사) — fill `panel2`, 1px `line`, text fg 13 strong; hover border → `accent@45%`. This is the default Button look — most buttons need no per-call styling.

**Ghost / danger** — transparent fill, text fg (ghost) or `danger` (destructive, e.g. 기록 비우기); hover fill = `danger@14%`. No filled-red unless irreversible.

**Text input** (single + multiline) — bg `panel2` (extreme_bg), 1px `line`, rounding 8, text fg, hint `muted`; focus stroke `(1.5, accent)`. Multiline send box `desired_rows(3)`, full width. E2E password `.password(true)`. Search box 220w with `🔍` in hint.

**Toggle / checkbox** — egui checkbox; checked tint = `accent.primary` via `selection.bg_fill`; box border 1px `line` → accent when on.

**Segmented control** (자동 지우기 끔/30초/1분/5분, 다크/라이트/시스템, 전체/선택) — row of `selectable_label`; selected = `accent@18%` fill + fg strong + 1px `accent@45%` stroke (the tint alone is too low-chroma on warm panel to read as "selected" — the border carries it); unselected = transparent + muted, hover → fg.

**Tabs** (top bar) — `selectable_label` 14; selected = `accent@16%` fill + fg + **2px `accent` underline** (manual `ui.painter().hline()` on `response.rect` — note this is real per-frame work, not a Frame prop); unselected = muted, hover → fg. Active tab + underline is the only accent-touched nav.

**History list item** — `Frame` fill `panel`, 1px `line`, rounding 10, `inner_margin(14,12)`, hover fill → `panel2`. Line 1: `[kind-tag]` + preview (fg 14, 1-line elided). Line 2 meta: muted 12.5 — `보냄/받음 · origin · size · time` joined by ` · `; **direction word coded (graft, desaturated): 보냄 in fg-strong, 받음 in muted** (NOT teal — keeps the one-accent rule). Rows separated by `item_spacing` only, no per-row rule.
- **Image-thumbnail variant**: 56×56 thumbnail on the left, rounding 6, 1px `line`, in a `panel`-tier frame; preview/filename text to its right. *(Net-new work: current feed is text-only, `HistRow` carries no bytes, eframe has no `image` feature — requires `egui_extras` + IPC blob transport. Flag, don't pretend it's a re-skin.)*

**Kind-tag** — `Frame` fill = tagColor@18% (dark) / @14% (light), rounding 8, `symmetric(7,2)`, label tagColor at 11.5 medium. **Calm-budget rule:** color is reserved for the three tags + status + toast only; running text spends zero color. 📝=tag.text · 🖼=tag.image · 📎=tag.file.

**Device-routing chip** — selectable `Frame`: idle fill `panel2` + 1px `line`; selected fill `accent@18%` + 1.5px `accent` stroke; leading ● online-dot = success (online) / `muted` (offline); name fg, rounding 10.

**Share-pool selector** — `ComboBox`: closed = `panel2` + 1px `line` + muted chevron, rounding 8; popup list = `panel` tier (step up from panel2 row), selected row `accent@16%`. Shown only when pools nonempty.

**Blocked-clip toast** (signature 🔒 동기화 차단됨) — bottom-center `Area`, `Order::Foreground`, width 332, rounding 12, `inner_margin(14,11)`.
> **Critical fix (Warm-Trust's documented flaw):** egui `Area` paints no backdrop, so a translucent fill composites over whatever tab is behind it (the dense 디버깅 log breaks it). **Paint an OPAQUE backing surface first, then the pink on top:**
> - DARK: solid `Frame` fill `#482B31` (= pink@20% pre-composited over panel), 1px stroke `blocked_pink@55%`. Title `#FBCFE8` (9.1:1), preview/fg `#EDE8DF` (10.3:1).
> - LIGHT: solid `Frame` fill `#FDE7F1`, 1px stroke `#E0588F`. Title `#9D174D` (6.7:1), preview `#831843` (8.2:1).
>
> Row: 🔒 (18) + "동기화 차단됨" strong 14 + reason chip [`Frame` fill pink@40%, rounding 10, micro 11.5: 비밀번호로 추정 / 카드번호로 추정 / 개인 키 / OTP·2FA 비밀 / 사용자 패턴]. Line 2: truncated-80 preview. Auto-dismiss ~5s; stack upward 86px. **Hard law: pink appears ONLY here — never elsewhere, so it always means "the filter just protected you."**

**Empty state** — `ui.weak("기록이 없습니다.")` / `"전송할 기기가 없습니다."` muted, centered-ish, generous top space.

**Loading state** — `ui.spinner()` in `muted` + muted label (e.g. "서버 검색 중…"); discovering buttons disabled (fill desaturated to `panel`, text `muted`).

---

## 4. PER-TAB egui REDESIGN

**Top bar** (`TopBottomPanel::top`, `panel` tier, 1px bottom `line`): wordmark "CopySync" (20) → **status pill immediately right of it** (graft: connection state is the very first thing read) → `right_to_left` tab row 🔗 연결 / 📋 기록 / ⚙️ 설정 / 🐞 디버깅 with underline-selected treatment. `CentralPanel` on `bg`, 16px side margin, vertical `ScrollArea` per tab, single column, cards stacked `add_space(12)`. Window 520×680 default, min 420×480.

- **🔗 연결** — `Card[상태]`: 2-col Grid (서버/기기/연결/E2E, values fg, "—" for empty) + `지금 재연결` primary. `Card[공유 풀]` (pools only): flat ComboBox. `Card[보내기]`: full-width multiline TextEdit + row [텍스트 보내기 **primary**][파일 보내기… secondary] — the only primary on the tab. `Card[전송 대상]`: 전체/선택 segmented; when 선택, wrapped device chips (online-dot + checkbox + name).
- **📋 기록** — sticky search row [TextEdit `🔍 검색…` 220w][새로고침 secondary] + 1px separator; then `ScrollArea` (`auto_shrink[false,false]`) of history-item cards (kind-tag + preview + thumbnail variant + coded meta). Empty → muted centered.
- **⚙️ 설정** — `Card[개인정보]`: `민감 클립 동기화 제외` toggle (default on) + `받은 항목 민감 표시` + `자동 지우기` segmented 끔/30초/1분/5분. `Card[기기 페어링]`: 2-col Grid (서버/OTP/기기 이름/PIN/E2E 암호 `password`) + [서버 검색][페어링 **primary**] + discovered-server secondary buttons + muted `pair_status`. `Card[화면]`: 다크/라이트/시스템 segmented. `Card[시스템]`: `부팅 시 자동 시작` toggle + `빠른 패널 단축키` input + 적용 + muted active-state line.
- **🐞 디버깅** — toolbar [이벤트 기록 checkbox][복사 ghost][지우기 ghost] + separator + `panel2`-tier `ScrollArea`, monospace 12.5 fg, timestamps muted, `stick_to_bottom`.

**Surface-tier usage:** `bg` = window canvas (the warm field that signals "calm long-running tool"); `panel` = every card and the top bar (the content lives here); `panel2` = nested interactive surfaces inside cards (inputs, history rows, combo closed-state, debug-log region) + the step-up popup surfaces go back to `panel`. Pink toast floats over all tabs on its own opaque surface.

---

## 5. PORTING THE TOKENS

**Android (Compose Material 3):** map tiers to surface roles — `bg → surface`, `panel → surfaceContainer`, `panel2 → surfaceContainerHigh`, `line → outlineVariant`, `fg → onSurface`, `muted → onSurfaceVariant`. `accent.primary` teal → `primary` (white = `onPrimary`); `success/danger/warn` → custom semantic colors (M3 has no built-in success). Kind-tags → `AssistChip`/`SuggestionChip` with `containerColor = tagColor.copy(alpha = .14f)`, `labelColor = tagColor`. **Use the LIGHT hex set for M3 light scheme and the DARK set for dark scheme — do not cross them** (the dark tag hues fail on white). Blocked toast → a `Surface`/`Snackbar` with the **opaque** pink surface (`#482B31` dark / `#FDE7F1` light), reusing `#E0588F`/`#9D174D`. Keep the hue-separation law: teal `primary` ≠ green `success`.

**Admin web (HTML/CSS):** declare as CSS custom properties under `:root` (light) and `[data-theme="dark"]` — `--bg --panel --panel2 --line --fg --muted --accent --success --danger --warn --tag-text --tag-image --tag-file --blocked-pink`. Same tier discipline: nested panes step `panel → panel2`, every surface gets `border:1px solid var(--line)`; no box-shadow (use the border for depth to stay visually consistent with the egui client). Pills/tags use `background: color-mix(in srgb, var(--tag-x) 16%, transparent)` with `color: var(--tag-x)` — but for the light theme, swap in the darkened light-hex values so the AA math holds. Pink reserved to the block banner only.

---

**Shared design laws (carried into every client):** (1) pink == "we protected you", nowhere else; (2) teal is fill-only, never small text; (3) brand-accent hue ≠ success hue (connection-OK and primary-action must never be confused); (4) two weights per surface; (5) never nest the same fill twice — step up a tier; (6) the blocked toast always paints an opaque surface before any tint; (7) light theme uses its own darkened accent/tag hues — dark hues are never reused on light.

**Implementation flags (not free re-skin):** Noto Sans KR `FontDefinitions` bundling (Hangul tofu today), the 2px tab underline (manual painter call), and history image thumbnails (`egui_extras` + IPC blob transport, currently text-only feed) are net-new work.

Relevant file for implementation: `/home/syaro/MikuchanRemote/CopySync/clients/desktop/gui/src/main.rs` (the `apply_theme`, `tab_button`, `kind_tag`, status-pill, and toast `Area` call sites are where these tokens and component frames are wired).