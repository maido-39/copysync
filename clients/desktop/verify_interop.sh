#!/usr/bin/env bash
# Headless end-to-end interop: Rust desktop core  <->  Go server  <->  copyctl,
# with E2E encryption on, proving the zero-knowledge relay.
set -e
export PATH=/home/syaro/.local/go/bin:$PATH
ROOT=/home/syaro/MikuchanRemote/CopySync
SRV=$ROOT/server
DESK=$ROOT/clients/desktop
INTEROP=$DESK/target/debug/examples/interop
PASS=secret-pass-한글
PORT=8473
W=$(mktemp -d /tmp/cs-interop.XXXXXX)
napp(){ timeout "$1" tail -f /dev/null 2>/dev/null || true; }
pass(){ echo "  PASS: $1"; }
fail(){ echo "  FAIL: $1"; FAILED=1; }
FAILED=0

echo "=== build go binaries ==="
( cd "$SRV" && go build -o "$W/copysyncd" ./cmd/copysyncd && go build -o "$W/copyctl" ./cmd/copyctl )
CTL="$W/copyctl"
echo "interop: $INTEROP"; [ -x "$INTEROP" ] || { echo "interop not built"; exit 1; }

echo "=== start server on :$PORT ==="
COPYSYNC_DATA_DIR=$W/data COPYSYNC_HTTPS_ADDR=":$PORT" COPYSYNC_SERVER_NAME="InteropSrv" \
  COPYSYNC_ADMIN_PASS=changeme "$W/copysyncd" >"$W/s.log" 2>&1 &
SPID=$!
trap 'kill $SPID 2>/dev/null; rm -rf "$W"' EXIT
curl -ks --retry 40 --retry-delay 1 --retry-connrefused https://127.0.0.1:$PORT/healthz >/dev/null
PIN=$(grep -oP 'spkiPin="\K[^"]+' "$W/s.log" | head -1)
SID=$(grep -oP ' id=\K[^ ]+' "$W/s.log" | head -1)
echo "  pin=$PIN"; echo "  serverId=$SID"
BASE="https://127.0.0.1:$PORT"

echo "=== admin: login, clear must-change-pw, mint 2 OTPs ==="
curl -ks -c "$W/jar" -X POST "$BASE/admin/login" -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"changeme"}' >/dev/null
curl -ks -b "$W/jar" -X POST "$BASE/admin/password" -H 'X-CopySync-CSRF: 1' -H 'Content-Type: application/json' \
  -d '{"current":"changeme","new":"changeme-strong-1"}' >/dev/null
mint_otp(){ curl -ks -b "$W/jar" -X POST "$BASE/admin/pairing" -H 'X-CopySync-CSRF: 1' | grep -oP '"otp":"\K[^"]+' | head -1; }
OTP1=$(mint_otp); OTP2=$(mint_otp)
echo "  otp1=$OTP1  otp2=$OTP2"

echo "=== pair goctl (copyctl) + rustdev (interop), same passphrase ==="
"$CTL" pair --server "$BASE" --otp "$OTP1" --name goctl --pin "$PIN" --e2e-pass "$PASS" --config "$W/goctl.json" >/dev/null
"$INTEROP" pair "$BASE" "$OTP2" rustdev "$W/rustdev.json" "$PASS" "$PIN"

echo
echo "=== TEST A: Rust --(E2E text)--> server --> copyctl ==="
MSG_A="hello-from-rust 한글-✓-$RANDOM"
"$CTL" watch --config "$W/goctl.json" --save-dir "$W/recvA" >"$W/watchA.log" 2>&1 &
WPID=$!; napp 2
ACK_A=$("$INTEROP" send "$W/rustdev.json" "$MSG_A" 2>&1)
napp 2; kill $WPID 2>/dev/null; wait $WPID 2>/dev/null || true
echo "$ACK_A" | grep -q "ack:relayed" && pass "Rust got ack:relayed" || fail "no relayed ack ($ACK_A)"
grep -qF "text: $MSG_A" "$W/watchA.log" && pass "copyctl decrypted Rust's text" || { fail "copyctl did not decrypt"; sed 's/^/    /' "$W/watchA.log"; }

echo
echo "=== TEST B: copyctl --(E2E text)--> server --> Rust ==="
MSG_B="hello-from-go 안녕-★-$RANDOM"
"$INTEROP" recv "$W/rustdev.json" 8 >"$W/recvB.log" 2>&1 &
RPID=$!; napp 2
"$CTL" send --text "$MSG_B" --config "$W/goctl.json" >"$W/sendB.log" 2>&1
napp 3; wait $RPID 2>/dev/null || true
grep -qF "CLIP text $MSG_B" "$W/recvB.log" && pass "Rust decrypted copyctl's text" || { fail "Rust did not decrypt"; sed 's/^/    /' "$W/recvB.log"; }

echo
echo "=== TEST C: Rust --(E2E file)--> server --> copyctl, integrity + zero-knowledge ==="
head -c 4096 /dev/urandom > "$W/blob.bin"
ORIG=$(sha256sum "$W/blob.bin" | cut -d' ' -f1)
"$CTL" watch --config "$W/goctl.json" --save-dir "$W/recvC" >"$W/watchC.log" 2>&1 &
WPID=$!; napp 2
"$INTEROP" sendfile "$W/rustdev.json" "$W/blob.bin" >"$W/sendC.log" 2>&1
napp 3; kill $WPID 2>/dev/null; wait $WPID 2>/dev/null || true
SAVED=$(ls "$W"/recvC/*.blob 2>/dev/null | head -1)
if [ -n "$SAVED" ]; then
  GOT=$(sha256sum "$SAVED" | cut -d' ' -f1)
  [ "$GOT" = "$ORIG" ] && pass "copyctl recovered identical file ($((${#ORIG}))-hex sha match)" || fail "file sha mismatch ($ORIG vs $GOT)"
else
  fail "copyctl saved no blob"; sed 's/^/    /' "$W/watchC.log"
fi
# Zero-knowledge: the blob the server stored must be ciphertext = plaintext+nonce(12)+tag(16).
STORED=$(find "$W/data" -type f -path '*blobs*' -size +0c | head -1)
if [ -n "$STORED" ]; then
  SZ=$(stat -c%s "$STORED")
  [ "$SZ" -eq 4124 ] && pass "server stored ciphertext only (4096+28=4124 bytes, never plaintext)" || fail "stored size $SZ != 4124 (expected ciphertext)"
  cmp -s "$STORED" "$W/blob.bin" && fail "server stored PLAINTEXT (zero-knowledge broken!)" || pass "stored bytes differ from plaintext"
else
  fail "no stored blob found on server"
fi

echo
echo "=== TEST D: wrong passphrase cannot decrypt (negative control) ==="
"$INTEROP" pair "$BASE" "$(mint_otp)" wrongdev "$W/wrong.json" "totally-different-pass" "$PIN" >/dev/null
"$CTL" watch --config "$W/goctl.json" --save-dir "$W/recvD" >"$W/watchD.log" 2>&1 &
WPID=$!; napp 2
"$INTEROP" send "$W/wrong.json" "should-not-decrypt-$RANDOM" >/dev/null 2>&1
napp 2; kill $WPID 2>/dev/null; wait $WPID 2>/dev/null || true
grep -q "e2e ciphertext" "$W/watchD.log" && pass "mismatched passphrase correctly failed to decrypt" || { fail "expected decrypt failure marker"; sed 's/^/    /' "$W/watchD.log"; }

echo
echo "=== TEST E: rich-text/HTML round-trip (Rust --(E2E text+html)--> server --> Rust) ==="
"$INTEROP" pair "$BASE" "$(mint_otp)" rusthtml "$W/rusthtml.json" "$PASS" "$PIN" >/dev/null
"$INTEROP" recv "$W/rusthtml.json" 8 >"$W/recvE.log" 2>&1 &
RPID=$!; napp 2
"$INTEROP" send "$W/rustdev.json" "bold-fallback-text" "<b>CSHTML</b> rich text" >/dev/null 2>&1
napp 3; wait $RPID 2>/dev/null || true
grep -q "CLIP text bold-fallback-text" "$W/recvE.log" && pass "plain-text fallback received" || { fail "no plain fallback"; sed 's/^/    /' "$W/recvE.log"; }
grep -q "html: <b>CSHTML</b> rich text" "$W/recvE.log" && pass "HTML variant decrypted end-to-end (server relays it opaquely)" || { fail "no html variant"; sed 's/^/    /' "$W/recvE.log"; }

echo
if [ "$FAILED" = 0 ]; then echo "ALL INTEROP TESTS PASSED ✓"; else echo "SOME TESTS FAILED ✗"; exit 1; fi
