#!/bin/sh
# Removes the Wake GRUB integration and regenerates grub.cfg.
#
#   sudo ./grub/uninstall.sh
set -eu

GRUB_CFG="${GRUB_CFG:-/boot/grub/grub.cfg}"

[ "$(id -u)" -eq 0 ] || { echo "uninstall.sh: run this with sudo" >&2; exit 1; }

if [ -e /etc/grub.d/06_wake ]; then
  rm -f /etc/grub.d/06_wake
  echo "Removed /etc/grub.d/06_wake"
else
  echo "/etc/grub.d/06_wake wasn't installed"
fi

if [ -e /etc/default/wake ]; then
  printf 'Also remove /etc/default/wake? [y/N]: '
  read -r reply || reply=""
  case "$reply" in
    y | Y | yes) rm -f /etc/default/wake && echo "Removed /etc/default/wake" ;;
    *) echo "Kept /etc/default/wake" ;;
  esac
fi

grub-mkconfig -o "$GRUB_CFG"
echo "GRUB is back to its own defaults (GRUB_DEFAULT in /etc/default/grub)."
