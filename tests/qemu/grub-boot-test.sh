#!/bin/sh
# Boots real GRUB (x86_64-efi) in QEMU with OVMF firmware and checks that the
# Wake snippet from grub/06_wake picks the right entry in each situation.
#
#   tests/qemu/grub-boot-test.sh            # all scenarios
#   tests/qemu/grub-boot-test.sh windows    # just one
#
# Needs: qemu-system-x86_64, edk2-ovmf, grub (grub-mkstandalone), python3.
# Nothing on the host's GRUB is touched: GRUB runs from a throwaway EFI image.
#
# QEMU user networking puts the guest on 10.0.2.0/24 with the host at
# 10.0.2.2, so GRUB reaches a Wake instance listening on the host's loopback.
set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="${WORK:-$ROOT/target/qemu-grub-test}"
OVMF_CODE="${OVMF_CODE:-/usr/share/edk2/x64/OVMF_CODE.4m.fd}"
OVMF_VARS="${OVMF_VARS:-/usr/share/edk2/x64/OVMF_VARS.4m.fd}"
WEB_PORT=18080
GRUB_PORT=18081
JUNK_PORT=18082
BOOT_TIMEOUT="${BOOT_TIMEOUT:-150}"

mkdir -p "$WORK"
WAKE_PID=""
JUNK_PID=""
cleanup() {
  [ -n "$WAKE_PID" ] && kill "$WAKE_PID" 2>/dev/null || true
  [ -n "$JUNK_PID" ] && kill "$JUNK_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

log() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

accel="tcg"
[ -w /dev/kvm ] && accel="kvm"

build_wake() {
  [ -x "$ROOT/target/release/wake" ] || (cd "$ROOT" && cargo build --release -q)
}

start_wake() {
  stop_wake
  rm -rf "$WORK/data"
  PC_NAME="QEMU" PC_MAC=52:54:00:12:34:56 WOL_BROADCAST=127.0.0.1 WOL_PORT=40009 \
    WEB_PORT=$WEB_PORT GRUB_PROTOCOL_PORT=$GRUB_PORT BIND_ADDR=127.0.0.1 \
    GRUB_ALLOWED_IPS=any DATA_DIR="$WORK/data" RUST_LOG=info,wake=debug \
    "$ROOT/target/release/wake" >"$WORK/wake.log" 2>&1 &
  WAKE_PID=$!
  for _ in $(seq 50); do
    curl -sf "http://127.0.0.1:$WEB_PORT/healthz" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "Wake didn't start:" >&2
  cat "$WORK/wake.log" >&2
  exit 1
}

stop_wake() {
  [ -n "$WAKE_PID" ] && kill "$WAKE_PID" 2>/dev/null && wait "$WAKE_PID" 2>/dev/null || true
  WAKE_PID=""
}

# Serves $1 as /grub/boot.env from a plain Python web server.
start_junk() {
  mkdir -p "$WORK/junk/grub"
  printf '%b' "$1" >"$WORK/junk/grub/boot.env"
  python3 -m http.server "$JUNK_PORT" --bind 127.0.0.1 --directory "$WORK/junk" >"$WORK/junk.log" 2>&1 &
  JUNK_PID=$!
  sleep 0.5
}

stop_junk() {
  [ -n "$JUNK_PID" ] && kill "$JUNK_PID" 2>/dev/null && wait "$JUNK_PID" 2>/dev/null || true
  JUNK_PID=""
}

# write_config PORT [extra lines...]
write_config() {
  port="$1"
  shift
  {
    echo 'WAKE_SERVER="10.0.2.2"'
    echo "WAKE_PORT=\"$port\""
    echo 'WAKE_LINUX_ENTRY="test-linux"'
    # A title with a space, to exercise the title form and the quoting.
    echo 'WAKE_WINDOWS_ENTRY="Test Windows"'
    for line in "$@"; do echo "$line"; done
  } >"$WORK/wake.conf"
}

build_efi() {
  {
    cat <<'EOF'
serial --unit=0 --speed=115200
terminal_input serial console
terminal_output serial console
set timeout=1
set timeout_style=menu
EOF
    WAKE_CONFIG="$WORK/wake.conf" sh "$ROOT/grub/06_wake"
    cat <<'EOF'
menuentry "Test Linux" --id test-linux {
  echo "WAKE-RESULT: linux"
  sleep 1
  halt
}
menuentry "Test Windows" --id test-windows {
  echo "WAKE-RESULT: windows"
  sleep 1
  halt
}
EOF
  } >"$WORK/grub.cfg"
  grub-script-check "$WORK/grub.cfg"
  rm -rf "$WORK/esp"
  mkdir -p "$WORK/esp/EFI/BOOT"
  grub-mkstandalone -O x86_64-efi \
    --modules="serial terminal efinet net http loadenv smbios echo halt sleep test normal" \
    -o "$WORK/esp/EFI/BOOT/BOOTX64.EFI" \
    "boot/grub/grub.cfg=$WORK/grub.cfg"
}

# boot NIC_MODE -> prints the result and the seconds it took
boot() {
  nic="$1"
  cp "$OVMF_VARS" "$WORK/vars.fd"
  if [ "$nic" = "none" ]; then
    netargs="-nic none"
  else
    # romfile= keeps QEMU's iPXE ROM out, so OVMF's own VirtioNetDxe provides
    # the SNP interface that GRUB's efinet uses (as on real firmware).
    # bootindex=1 (after the disk) matters: OVMF only connects drivers for
    # devices in the boot order, just as many real boards only start their
    # UEFI network stack when network boot is enabled.
    netargs="-netdev user,id=n0 -device virtio-net-pci,netdev=n0,romfile=,bootindex=${NIC_BOOTINDEX:-1}"
  fi
  rm -f "$WORK/serial.log"
  start=$(date +%s)
  # shellcheck disable=SC2086
  timeout "$BOOT_TIMEOUT" qemu-system-x86_64 \
    -machine q35,accel=$accel -m 256 -display none -no-reboot \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file="$WORK/vars.fd" \
    -drive if=none,id=esp,format=raw,file=fat:rw:"$WORK/esp" \
    -device virtio-blk-pci,drive=esp,bootindex=0 \
    $netargs \
    -serial file:"$WORK/serial.log" >/dev/null 2>&1 || true
  elapsed=$(( $(date +%s) - start ))
  result=$(grep -ao 'WAKE-RESULT: [a-z]*' "$WORK/serial.log" | tail -n 1 | cut -d' ' -f2)
  echo "${result:-none} $elapsed"
}

PASS=0
FAIL=0
# expect NAME EXPECTED "RESULT SECONDS"
expect() {
  name="$1"
  want="$2"
  got="${3% *}"
  secs="${3#* }"
  if [ "$got" = "$want" ]; then
    PASS=$((PASS + 1))
    printf '  PASS  %-28s booted %-8s in %ss\n' "$name" "$got" "$secs"
  else
    FAIL=$((FAIL + 1))
    printf '  FAIL  %-28s wanted %s, got %s (%ss); serial log: %s\n' "$name" "$want" "$got" "$secs" "$WORK/serial-$name.log"
  fi
  cp "$WORK/serial.log" "$WORK/serial-$name.log" 2>/dev/null || true
}

api() { curl -sf -X POST "http://127.0.0.1:$WEB_PORT$1" >/dev/null; }
status_field() {
  curl -sf "http://127.0.0.1:$WEB_PORT/api/status" | python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"
}

scenario_windows() {
  log "Windows chosen: GRUB must boot Windows and use the choice up"
  start_wake
  api /api/boot/windows
  write_config $GRUB_PORT
  build_efi
  expect windows windows "$(boot net)"
  consumed=$(status_field "d['next_boot']['explicit']")
  served=$(status_field "d['last_boot']['os'] if d['last_boot'] else None")
  if [ "$consumed" = "False" ] && [ "$served" = "windows" ]; then
    PASS=$((PASS + 1)); echo "  PASS  choice consumed              last_boot=windows, next_boot back to default"
  else
    FAIL=$((FAIL + 1)); echo "  FAIL  choice consumed              explicit=$consumed last_boot=$served"
  fi
  grep -c 'GET\|answered GRUB\|repeat' "$WORK/wake.log" | xargs printf '        (%s GRUB request lines in wake.log)\n'
}

scenario_linux_default() {
  log "Nothing chosen: GRUB must boot Linux"
  start_wake
  write_config $GRUB_PORT
  build_efi
  expect default linux "$(boot net)"
}

scenario_linux_chosen() {
  log "Linux chosen explicitly"
  start_wake
  api /api/boot/linux
  write_config $GRUB_PORT
  build_efi
  expect linux-chosen linux "$(boot net)"
}

scenario_server_down() {
  log "Wake not running: GRUB must fall back to Linux"
  stop_wake
  write_config $GRUB_PORT
  build_efi
  expect server-down linux "$(boot net)"
}

scenario_no_nic() {
  log "No network card: GRUB must boot Linux"
  start_wake
  api /api/boot/windows
  write_config $GRUB_PORT
  build_efi
  expect no-nic linux "$(boot none)"
}

scenario_static() {
  log "Static address instead of DHCP, Windows chosen"
  start_wake
  api /api/boot/windows
  write_config $GRUB_PORT 'WAKE_NET="static"' 'WAKE_STATIC_IP="10.0.2.15"' 'WAKE_NET_CARD="efinet0"'
  build_efi
  expect static windows "$(boot net)"
}

scenario_junk() {
  log "Something else answers: garbage and hostile env blocks must not pick Windows"
  stop_wake
  write_config $JUNK_PORT
  build_efi
  start_junk '1\n'
  expect junk-plain-1 linux "$(boot net)"
  stop_junk
  start_junk '# GRUB Environment Block\nwake_boot=banana\n'
  expect junk-banana linux "$(boot net)"
  stop_junk
  # Tries to set GRUB's default directly; the load_env whitelist must ignore it.
  start_junk '# GRUB Environment Block\ndefault=test-windows\nwake_boot=0\n'
  expect junk-sets-default linux "$(boot net)"
  stop_junk
  # A well-formed block from a foreign server with the right name does work,
  # which is why the README says GRUB's plain HTTP is only as trusted as your LAN.
  start_junk '# GRUB Environment Block\nwake_boot=1\n'
  expect foreign-valid-block windows "$(boot net)"
  stop_junk
}

build_wake
echo "accel=$accel, work dir $WORK"
if [ $# -eq 0 ]; then
  set -- windows linux_default linux_chosen static no_nic junk server_down
fi
for s in "$@"; do "scenario_$s"; done

echo ""
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
