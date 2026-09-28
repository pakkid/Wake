#!/bin/sh
# Installs the Wake GRUB integration on an Arch Linux (x86_64 EFI) machine.
#
#   sudo ./grub/install.sh
#
# It will:
#   1. write /etc/default/wake (only if it doesn't exist), after detecting your
#      Linux and Windows menu entries and asking you to confirm them,
#   2. install /etc/grub.d/06_wake,
#   3. syntax-check the generated GRUB script with grub-script-check,
#   4. back up grub.cfg and regenerate it with grub-mkconfig.
#
# Nothing is changed until you confirm. Override paths with GRUB_CFG=... if
# your grub.cfg isn't /boot/grub/grub.cfg.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
GRUB_CFG="${GRUB_CFG:-/boot/grub/grub.cfg}"
CONFIG=/etc/default/wake
GENERATOR=/etc/grub.d/06_wake

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
ask() {
  # ask "Question" default -> answer on stdout
  printf '%s [%s]: ' "$1" "$2" >&2
  read -r reply || reply=""
  printf '%s' "${reply:-$2}"
}
confirm() {
  printf '%s [y/N]: ' "$1" >&2
  read -r reply || reply=""
  case "$reply" in y | Y | yes | YES) return 0 ;; *) return 1 ;; esac
}

[ "$(id -u)" -eq 0 ] || die "run this with sudo"
for tool in grub-mkconfig grub-script-check; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool not found (is the grub package installed?)"
done
[ -r "$GRUB_CFG" ] || die "can't read $GRUB_CFG (set GRUB_CFG=/path/to/grub.cfg)"
[ -d /sys/firmware/efi ] || say "Warning: this system didn't boot in EFI mode. Wake needs x86_64-efi GRUB."

for mod in efinet http loadenv; do
  if [ ! -e "/boot/grub/x86_64-efi/$mod.mod" ] && [ ! -e "/usr/lib/grub/x86_64-efi/$mod.mod" ]; then
    die "GRUB module $mod.mod is missing"
  fi
done

# The first menuentry that looks like the Linux install, preferring its id.
detect_linux() {
  grep -Eo "gnulinux-[A-Za-z0-9._-]+" "$GRUB_CFG" | grep -v -- '-recovery-' | head -n 1
}

# The menuentry whose body chainloads Windows Boot Manager: its id if it has
# one, otherwise its title.
detect_windows() {
  awk '
    /^[[:space:]]*menuentry[[:space:]]/ {
      line = $0; title = ""; id = ""
      if (match(line, /menuentry[[:space:]]+["\047][^"\047]*["\047]/)) {
        t = substr(line, RSTART, RLENGTH); sub(/menuentry[[:space:]]+["\047]/, "", t); sub(/["\047]$/, "", t); title = t
      }
      if (match(line, /(--id|\$menuentry_id_option)[[:space:]]+["\047]?[^"\047[:space:]{]+/)) {
        i = substr(line, RSTART, RLENGTH); sub(/^(--id|\$menuentry_id_option)[[:space:]]+["\047]?/, "", i); id = i
      }
      inside = 1; next
    }
    inside && /bootmgfw\.efi/ { print (id != "" ? id : title); exit }
    inside && /^[[:space:]]*}[[:space:]]*$/ { inside = 0 }
  ' "$GRUB_CFG"
}

if [ -e "$CONFIG" ]; then
  say "Keeping your existing $CONFIG:"
  sed 's/^/  /' "$CONFIG"
else
  linux_entry="$(detect_linux || true)"
  windows_entry="$(detect_windows || true)"
  say "Detected in $GRUB_CFG:"
  say "  Linux entry:   ${linux_entry:-<not found>}"
  say "  Windows entry: ${windows_entry:-<not found>}"
  say ""
  server="$(ask "IPv4 address of the machine running Wake" "${WAKE_SERVER:-192.168.1.10}")"
  port="$(ask "Wake's GRUB_PROTOCOL_PORT" "${WAKE_PORT:-8081}")"
  linux_entry="$(ask "Linux menu entry (id or exact title)" "$linux_entry")"
  windows_entry="$(ask "Windows menu entry (id or exact title)" "$windows_entry")"
  [ -n "$linux_entry" ] && [ -n "$windows_entry" ] || die "both menu entries are needed"
  default_os="$(ask "Boot which OS when Wake has no choice or can't be reached? (linux/windows)" "${WAKE_DEFAULT:-linux}")"
  case "$default_os" in linux | windows) ;; *) die "answer linux or windows" ;; esac

  tmp="$(mktemp)"
  trap 'rm -f "$tmp"' EXIT
  sed \
    -e "s|^WAKE_SERVER=.*|WAKE_SERVER=\"$server\"|" \
    -e "s|^WAKE_PORT=.*|WAKE_PORT=\"$port\"|" \
    -e "s|^WAKE_LINUX_ENTRY=.*|WAKE_LINUX_ENTRY=\"$linux_entry\"|" \
    -e "s|^WAKE_WINDOWS_ENTRY=.*|WAKE_WINDOWS_ENTRY=\"$windows_entry\"|" \
    -e "s|^WAKE_DEFAULT=.*|WAKE_DEFAULT=\"$default_os\"|" \
    "$HERE/wake.default" > "$tmp"
  say ""
  say "This will be written to $CONFIG:"
  grep -E '^WAKE_' "$tmp" | sed 's/^/  /'
  confirm "Write it?" || die "nothing changed"
  install -m 0644 "$tmp" "$CONFIG"
fi

say ""
say "Checking the generated GRUB script..."
preview="$(mktemp)"
WAKE_CONFIG="$CONFIG" sh "$HERE/06_wake" > "$preview"
grub-script-check "$preview" || die "grub-script-check rejected the script; nothing installed"
sed 's/^/  | /' "$preview"
rm -f "$preview"

say ""
confirm "Install $GENERATOR and regenerate $GRUB_CFG?" || die "stopped before touching GRUB"
install -m 0755 "$HERE/06_wake" "$GENERATOR"
backup="$GRUB_CFG.before-wake"
cp -p "$GRUB_CFG" "$backup"
say "Backed up $GRUB_CFG to $backup"
grub-mkconfig -o "$GRUB_CFG"

say ""
say "Done. Next steps:"
say "  1. Make sure the firmware's UEFI network stack is enabled (BIOS: 'Network Stack' / 'IPv4 PXE Support'),"
say "     and that network (PXE) boot is in the boot order after the disk: many boards only start the"
say "     NIC driver for devices in that list."
say "  2. Set the server's DEFAULT_BOOT to '$(. "$CONFIG"; echo "${WAKE_DEFAULT:-linux}")' so both sides agree."
say "  3. Reboot and watch for 'Wake: asking $(. "$CONFIG"; echo "$WAKE_SERVER:${WAKE_PORT:-8081}") for the next boot...'"
say "  4. To undo: sudo $HERE/uninstall.sh"
