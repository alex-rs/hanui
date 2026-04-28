#!/usr/bin/env bash
# Boot the hanui dev VM (Debian 12 cloud image, KVM-accelerated).
# SSH in:    ssh -p 2222 -i vm/keys/id_ed25519 dev@127.0.0.1
# Or:        ./ssh-vm.sh
set -euo pipefail

cd "$(dirname "$0")"

DISK="vm/disk.qcow2"
SEED="vm/seed.iso"
RAM="${RAM:-2048}"
CPUS="${CPUS:-2}"
SSH_PORT="${SSH_PORT:-2222}"
DISPLAY_MODE="${DISPLAY_MODE:-none}"   # set to "gtk" for a window, "none" for headless

[[ -f "$DISK" ]] || { echo "missing $DISK — run setup first" >&2; exit 1; }
[[ -f "$SEED" ]] || { echo "missing $SEED — run setup first" >&2; exit 1; }

ACCEL_ARGS=()
if [[ -r /dev/kvm && -w /dev/kvm ]]; then
  ACCEL_ARGS=(-machine type=q35,accel=kvm -cpu host -enable-kvm)
else
  echo "warning: /dev/kvm not accessible — falling back to TCG (slow)" >&2
  ACCEL_ARGS=(-machine type=q35 -cpu max)
fi

# Two host-loopback forwards on the user-mode SLIRP stack:
#   - $SSH_PORT (default 2222) -> :22  : SSH into the VM
#   - 8123                     -> :8123: host-side `cargo run` (Phase 2.5) reaches
#                                        the in-VM HA Core container. The compose
#                                        file binds the container's 8123 to the VM's
#                                        127.0.0.1:8123 only, so this hostfwd plus
#                                        the compose mapping is end-to-end loopback.
# Both are bound to host loopback (127.0.0.1) only — nothing on the host's
# external interface is exposed. Same defense-in-depth posture as the SSH
# forward. Per TASK-055 founder smoke 2026-04-28 + TASK-057.
exec qemu-system-x86_64 \
  "${ACCEL_ARGS[@]}" \
  -smp "$CPUS" \
  -m "$RAM" \
  -drive file="$DISK",if=virtio,format=qcow2 \
  -drive file="$SEED",if=virtio,format=raw,readonly=on \
  -netdev user,id=net0,hostfwd=tcp:127.0.0.1:"$SSH_PORT"-:22,hostfwd=tcp:127.0.0.1:8123-:8123 \
  -device virtio-net-pci,netdev=net0 \
  -device virtio-rng-pci \
  -nographic \
  -display "$DISPLAY_MODE" \
  -serial mon:stdio
