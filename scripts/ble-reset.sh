#!/usr/bin/env bash
# Reset the Bluetooth adapter from the host (NOT in a container).
# The LE controller accumulates zombie connections when clients are
# SIGKILLed mid-connection; the stack then degrades to NotReady: scans
# return nothing and GATT connects D-Bus-timeout. A kernel-level power
# cycle clears the controller's connection table.
set -euo pipefail
if ! sudo -n true 2>/dev/null; then
    echo "need passwordless sudo for btmgmt (or run: sudo $0)" >&2
    exit 1
fi
sudo -n btmgmt power off
sleep 1
sudo -n btmgmt power on
echo "adapter power-cycled. If scans still return nothing, try:"
echo "  sudo modprobe -r btusb btintel btbcm && sudo modprobe btusb"
