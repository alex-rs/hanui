---
name: kiosk-runtime
description: Configure the native Slint dashboard to run on boot in fullscreen on DietPi, Raspberry Pi OS Lite, or Armbian using minimal systemd/Wayland/X11 runtime.
allowed-tools: Read, Write, Edit, MultiEdit, Glob, Grep, Bash
---

# Kiosk Runtime

## Purpose

Deploy the native dashboard to an SBC as a fullscreen appliance.

Targets: DietPi, Raspberry Pi OS Lite, Armbian, Debian minimal, Ubuntu Server minimal. Hardware: Raspberry Pi 4 4GB and Orange Pi Zero 3 2GB.

## Runtime goal

Boot directly into the dashboard without a desktop environment: systemd → kiosk user → minimal display backend → native Slint app fullscreen.

## Backend preference

1. Wayland + cage
2. Weston kiosk-shell
3. minimal X11 fallback
4. direct DRM/KMS only if the app backend supports it

Do not install a full desktop environment.

## systemd service

```ini
[Unit]
Description=Home Assistant Native Dashboard
After=network-online.target
Wants=network-online.target

[Service]
User=kiosk
Group=kiosk
WorkingDirectory=/opt/ha-native-dashboard
Environment=RUST_LOG=info
ExecStart=/usr/bin/cage -s -- /opt/ha-native-dashboard/ha-native-dashboard --fullscreen
Restart=always
RestartSec=2
WatchdogSec=30
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

## User setup

```bash
sudo useradd -r -m -s /usr/sbin/nologin kiosk
sudo mkdir -p /opt/ha-native-dashboard
sudo chown -R kiosk:kiosk /opt/ha-native-dashboard
sudo usermod -aG video,render,input,tty kiosk
```

## DietPi notes

Start minimal, do not install desktop, install only display/backend dependencies, disable unused services through `dietpi-services`, use `dietpi-config` for GPU/network basics, use systemd for the app.

## Packages

Wayland/cage:

```bash
sudo apt update
sudo apt install --no-install-recommends cage seatd libinput10 libwayland-client0 libxkbcommon0 fonts-dejavu-core
```

Weston fallback:

```bash
sudo apt install --no-install-recommends weston seatd libinput10 fonts-dejavu-core
```

X11 fallback:

```bash
sudo apt install --no-install-recommends xserver-xorg-core xinit openbox fonts-dejavu-core
```

## Anti-patterns

Do not run as root, install GNOME/KDE/LXDE, start a browser, depend on shell profiles in production, leave crash loops without logs, or expose input devices unnecessarily.
