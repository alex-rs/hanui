---
name: vm
description: Manage the hanui local QEMU/KVM Debian 12 dev VM. Use when the user wants to boot/shut down the VM, ssh into it, run a command inside it, reset the disk to a clean state, rebuild the cloud-init seed, or check VM status. Subcommands accepted via args - "up", "down", "ssh [cmd...]", "status", "reset", "seed".
---

# hanui VM skill

The project ships a self-contained QEMU/KVM Debian 12 (bookworm) dev VM under `vm/`. Treat this skill as the single entry point for VM lifecycle operations - do not improvise alternative qemu invocations.

## Layout (already present in project root)

- `run-vm.sh` - boots the VM (KVM, headless serial, port-forward `127.0.0.1:2222 → 22`). Env knobs: `RAM`, `CPUS`, `SSH_PORT`, `DISPLAY_MODE` (`none`|`gtk`).
- `ssh-vm.sh` - SSH wrapper using `vm/keys/id_ed25519` as user `dev`, passes args through.
- `vm/base.qcow2` - pristine downloaded Debian image (do not write to it).
- `vm/disk.qcow2` - the working disk used at runtime.
- `vm/seed.iso` - cloud-init NoCloud seed.
- `vm/cloud-init/{user-data,meta-data}` - cloud-init source.
- `vm/keys/id_ed25519{,.pub}` - dedicated VM key (gitignored).
- `vm/boot.log` - last boot's serial console output (only when started via `up`).

User inside the VM is `dev` with sudo NOPASSWD. Hostname is `hanui`. Disk is 20G, RAM 2G, 2 vCPU by default.

## Dispatch

Look at the args. If empty, treat as `up`. Run only what the subcommand calls for.

### `up` - start the VM in background

1. Check it isn't already running: `pgrep -af '/qemu-system-x86_64' | grep -q disk.qcow2 && echo RUNNING`. If running, report and stop.
2. Sanity-check artifacts exist: `vm/disk.qcow2` and `vm/seed.iso`. If either is missing, tell the user to run the `reset` subcommand (for the disk) or `seed` subcommand (for the seed) and stop.
3. Boot in background with output to `vm/boot.log`:
   ```
   cd /home/alex/Code/hanui && nohup ./run-vm.sh > vm/boot.log 2>&1 & disown
   ```
4. Wait for SSH to come up (cloud-init may run apt on a fresh disk - up to ~3 min):
   ```
   until ssh -p 2222 -i vm/keys/id_ed25519 -o UserKnownHostsFile=vm/known_hosts \
       -o StrictHostKeyChecking=accept-new -o ConnectTimeout=3 -o LogLevel=ERROR \
       -o BatchMode=yes dev@127.0.0.1 true 2>/dev/null; do sleep 5; done
   ```
   Use Bash with `run_in_background=true` and a 300000ms timeout. Wait for the completion notification - do not poll.
5. Once SSH is up, run `cloud-init status` inside the VM to confirm it reports `done` (not `error`). On error, surface the failing module and `/var/log/cloud-init-output.log` tail.

### `down` - shut down cleanly

1. Send `sudo poweroff` via `./ssh-vm.sh`.
2. Wait up to 15s for the qemu process to exit: poll `pgrep -af '/qemu-system-x86_64' | grep -q disk.qcow2`.
3. If still running after 15s, surface that and ask the user before sending SIGTERM.

### `ssh [cmd...]` - run inside the VM (or open a shell)

- With args: `./ssh-vm.sh <cmd...>` and surface the result.
- No args: tell the user to run `./ssh-vm.sh` themselves (interactive shells don't work through Bash tool). Do not try to allocate a TTY.

### `status` - report state

Print: qemu process status, SSH reachability on 2222, host and guest disk usage if SSH is up, `cloud-init status` if SSH is up. Keep it to ~10 lines.

### `reset` - recreate disk from base

Destructive. Confirm with the user first unless they already explicitly asked to reset.

1. If VM is running, run the `down` flow first.
2. `rm -f vm/disk.qcow2 vm/known_hosts`
3. `qemu-img convert -O qcow2 vm/base.qcow2 vm/disk.qcow2`
4. `qemu-img resize vm/disk.qcow2 20G`
5. Tell the user the disk is clean and the next `up` will re-run cloud-init (~2-3 min).

### `seed` - rebuild cloud-init seed.iso

Use after editing `vm/cloud-init/user-data` or `meta-data`.

1. `genisoimage -output vm/seed.iso -volid cidata -joliet -rock vm/cloud-init/user-data vm/cloud-init/meta-data`
2. Note: an already-provisioned disk will NOT re-run cloud-init on the new seed - it's keyed by `instance-id`. To apply seed changes, run `reset` then `up`.

## Guardrails

- Never delete `vm/base.qcow2` or `vm/keys/` without explicit user instruction - re-downloading the image and re-keying is a real cost.
- Never edit `run-vm.sh` from inside this skill; if the user wants different qemu flags, surface the proposed diff and ask.
- KVM access is via ACL on `/dev/kvm` (user `alex`), not group membership. If `/dev/kvm` is not accessible, `run-vm.sh` falls back to TCG and warns - surface that warning to the user, do not silently proceed.
- The VM listens only on `127.0.0.1:2222` by design. If the user asks to expose it on the LAN, point out they'd need to change `run-vm.sh`'s `hostfwd` and confirm.
