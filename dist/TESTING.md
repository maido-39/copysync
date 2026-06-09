# CopySync 실기기 테스트 가이드 (Server · Windows · Android)

이 문서는 **서버 1대 + Windows 데스크톱 + Android 폰**으로 실제 동기화를 검증하는
순서입니다. 모든 기기는 같은 LAN(또는 같은 서브넷)에 있어야 합니다.
(WireGuard 등 다른 서브넷이면 서버 주소를 IP로 직접 입력해야 합니다 — mDNS 검색은
링크-로컬이라 터널을 못 넘습니다.)

---

## 0. 설치 파일 받기

이 빌드 머신이 LAN에 파일을 서빙 중입니다:

> **http://192.168.20.177:8099/**

| 파일 | 용도 |
|---|---|
| `copysync-android.apk` | Android 앱 (디버그 서명, 사이드로드용) |
| `copysyncd-linux-amd64` / `-windows-amd64.exe` / `-macos-arm64` | 서버 (원하는 OS) |
| `copyctl-linux-amd64` / `-windows-amd64.exe` | CLI 클라이언트(선택, 추가 검증용) |
| `copysync-desktop-windows-x64.exe` | Windows 데스크톱 앱(빠른 테스트용, 빌드 완료 후 게시) |

**Windows 정식 설치파일(.msi/.exe)** 은 `v0.1.0` 태그 푸시로 GitHub Actions가 빌드 중입니다:
**github.com/maido-39/copysync → Actions → "desktop" 실행 → Artifacts → `copysync-windows`**
(WiX MSI + NSIS exe). 서명이 안 돼 SmartScreen 경고가 날 수 있습니다(→ 추가 정보 → 실행).

---

## 1. 서버 실행

서버는 LAN의 한 대(PC/노트북/리눅스 박스)에서 돌립니다. 둘 중 하나:

### 방법 A — Docker (권장)
```bash
git clone git@github.com:maido-39/copysync.git
cd copysync
docker compose up --build -d
```

### 방법 B — 바이너리 (Docker 없이)
리눅스/맥:
```bash
chmod +x copysyncd-linux-amd64
COPYSYNC_SERVER_NAME="home-server" \
COPYSYNC_DATA_DIR=./csdata \
COPYSYNC_HTTPS_ADDR=:8443 \
./copysyncd-linux-amd64
```
Windows(서버를 윈도우에서 돌릴 경우, PowerShell):
```powershell
$env:COPYSYNC_SERVER_NAME="home-server"; $env:COPYSYNC_DATA_DIR=".\csdata"; $env:COPYSYNC_HTTPS_ADDR=":8443"
.\copysyncd-windows-amd64.exe
```

- 기동 로그에 `spkiPin="..."` 와 `id=srv_...` 가 찍힙니다. **spkiPin** 을 메모해 두면
  페어링 때 핀을 직접 넣어 TOFU(최초접속 신뢰)를 건너뛸 수 있습니다.
- **방화벽**: 8443/TCP 인바운드를 허용하세요(Windows면 첫 실행 시 방화벽 팝업 → 허용).
- 서버 머신의 LAN IP 확인: 리눅스 `hostname -I`, Windows `ipconfig`. 이걸 `<SERVER_IP>`로 씁니다.

### 관리자 + OTP 발급
1. 브라우저로 **https://<SERVER_IP>:8443** 접속 → 자체서명 인증서 경고 수락.
2. 로그인 **admin / changeme** → 비밀번호 강제 변경.
3. **기기 페어링 → 페어링 코드 생성** → OTP(8자리) + QR이 뜸. QR에는 서버 주소·SPKI 핀·OTP가 들어있습니다.
4. (선택) **종단간 암호화(E2E)** 를 쓰려면, 모든 기기에서 **동일한 암호문(passphrase)** 을
   페어링 시 입력하세요. 서버는 내용을 못 봅니다(암호문만 중계).

---

## 2. Android 테스트

Android는 보안상 백그라운드 클립보드 읽기를 막으므로, 검증된 우회(권한 부여)가 한 번 필요합니다.

```bash
# 1) 설치 (USB 디버깅 ON + adb 연결, 또는 폰 브라우저로 위 URL의 apk 다운로드)
adb install -r copysync-android.apk

# 2) 백그라운드 캡처 권한 (ADB 또는 Shizuku로만 가능)
adb shell pm grant com.copysync.android android.permission.READ_LOGS
adb shell appops set com.copysync.android SYSTEM_ALERT_WINDOW allow

# 3) ★중요: 앱을 강제종료 후 다시 실행 (logcat 구독이 READ_LOGS를 상속해야 함)
adb shell am force-stop com.copysync.android
```
- adb가 없으면: 폰 브라우저로 `http://192.168.20.177:8099/copysync-android.apk` 받아 설치 →
  Shizuku로 같은 권한 2개 부여 → 앱 재실행.

### 페어링 + 사용
1. 앱 실행 → 하단 **페어링** 탭.
2. **같은 네트워크에서 서버 검색** 버튼으로 서버를 찾거나, **서버 주소**에
   `https://<SERVER_IP>:8443` 직접 입력.
3. **OTP** 입력, 기기 이름 지정, (E2E 쓰면) **암호문** 입력 → **페어링**.
4. **연결** 탭이 초록색 "● 연결됨" 이 되면 OK. 상태바에 포그라운드 알림이 떠 있어야 백그라운드 동기화가 동작합니다.

---

## 3. Windows 데스크톱 테스트

### 방법 A — 정식 설치파일(MSI/NSIS, 권장)
GitHub Actions(위 0번)에서 받은 `.msi` 실행 → 설치. WebView2는 Win10/11에 기본 포함.

### 방법 B — 단일 exe (빠른 테스트)
**`copysync-desktop-windows-x64.zip`** 다운로드 → 압축 풀기 → 같은 폴더의 exe 실행.
- zip 안에 `WebView2Loader.dll`이 함께 들어 있습니다. 이 빠른-테스트 exe는 gnu 크로스
  빌드라 로더 DLL이 정적 링크되지 않아 **exe 옆에 DLL이 있어야** 합니다(정식 MSI는 불필요).
- `WebView2Loader.dll이(가) 없어…` 오류가 났다면 → zip을 쓰거나, `WebView2Loader.dll`
  하나만 받아 exe와 같은 폴더에 두세요.
- 그 다음 WebView2 *런타임* 관련 오류가 나면 **Edge WebView2 Runtime(Evergreen)** 설치(Win10/11 보통 기본 포함).
- SmartScreen 경고(미서명) → **추가 정보 → 실행**.
- gnu 크로스빌드라 미묘한 문제가 있으면, 깔끔한 경로는 **CI의 .msi**(위 0번)입니다.

### 페어링 + 사용
앱의 **페어링** 탭에서 서버 주소 + OTP (+선택: 핀, E2E 암호문) 입력. 페어링 후 자동 연결.
창을 닫으면 **트레이로 최소화**되고 동기화는 계속됩니다(트레이 아이콘 우클릭 → 종료).
**설정** 탭에서 부팅 시 자동 시작을 켤 수 있습니다.

---

## 4. 테스트 매트릭스 (양방향으로 확인)

| 항목 | 방법 | 기대 결과 |
|---|---|---|
| 텍스트 | A기기에서 텍스트 복사 | B기기 클립보드에 즉시 반영 + 알림 |
| 리치텍스트/HTML | 브라우저/문서에서 서식 있는 텍스트 복사 | B기기에 붙여넣기 시 서식 유지 |
| 이미지 | 이미지 복사(또는 캡처) | B기기 클립보드에 이미지로 반영 |
| 파일(소형) | 데스크톱 "파일 보내기" / Android 공유→CopySync | B기기에서 받기/저장 |
| 파일(대용량) | 임계값 초과 파일 전송 | "받기" 누를 때만 다운로드(온디맨드) |
| 라우팅 | "전송 대상"에서 특정 기기만 선택 | 선택한 기기에만 전달 |
| 오프라인 큐 | B기기 끄고 A에서 복사 → B 켜기 | 재접속 시 밀린 항목 수신 |
| mDNS 검색 | 같은 LAN에서 "검색"/`copyctl discover` | 서버가 목록에 뜸 |
| E2E | 모든 기기 같은 암호문, 다른 기기엔 다른 암호문 | 같은 암호문끼리만 복호화; 서버 admin은 내용 못 봄 |

### (선택) CLI로 교차 검증
```bash
chmod +x copyctl-linux-amd64
./copyctl-linux-amd64 pair --server https://<SERVER_IP>:8443 --otp <OTP> --name laptop [--e2e-pass <암호문>]
./copyctl-linux-amd64 watch          # 들어오는 클립 출력/저장
./copyctl-linux-amd64 send --text "hello from cli"
./copyctl-linux-amd64 discover       # mDNS로 서버 찾기
```

---

## 5. 트러블슈팅

- **Android에서 내가 복사한 게 안 올라감** → READ_LOGS/오버레이 권한 부여 후 **앱 재실행** 했는지 확인(가장 흔한 원인).
- **연결이 안 됨** → 서버 8443 포트 방화벽, `<SERVER_IP>` 정확한지, 같은 서브넷인지 확인.
- **핀 불일치(MITM 경고)** → 서버 데이터 볼륨을 지우면 인증서/핀이 재생성됩니다. 그러면 모든 기기 재페어링 필요.
- **다른 서브넷/WireGuard** → mDNS 검색 대신 서버 IP를 직접 입력.
- **E2E인데 "복호화 불가"** → 기기들 암호문이 동일한지 확인.
