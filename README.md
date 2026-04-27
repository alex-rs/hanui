# HA Native Slint Claude Kit

Claude Code kit for building a native Rust + Slint Home Assistant dashboard for low-power SBCs.

## Included skills

```text
.claude/skills/
  project-architecture/
  html-to-slint-widget/
  ha-state-engine/
  ha-action-dispatcher/
  dashboard-layout-engine/
  theme-and-design-tokens/
  kiosk-runtime/
  render-optimization/
```

The 7 core skills are `html-to-slint-widget`, `ha-state-engine`, `ha-action-dispatcher`, `dashboard-layout-engine`, `theme-and-design-tokens`, `kiosk-runtime`, and `render-optimization`. `project-architecture` is included as an umbrella skill.

## Included agent

```text
.claude/agents/ha-native-dashboard-architect.md
```

## Included docs

```text
docs/ARCHITECTURE.md
docs/DASHBOARD_SCHEMA.md
docs/ROADMAP.md
```

## Install

Copy `.claude/` into your project root and restart/reload Claude Code.

## Example prompts

```text
Use the ha-native-dashboard-architect agent to create the initial Rust + Slint project skeleton from docs/ARCHITECTURE.md.
Use the ha-state-engine skill to implement a fixture-backed entity store.
Use the html-to-slint-widget skill to port this Lovelace light card to Slint.
Use the kiosk-runtime skill to add DietPi systemd deployment files for Raspberry Pi 4 and Orange Pi Zero 3.
```

## Local HA target

`make ha-up` provisions a local Home Assistant Core Docker instance inside the dev VM,
declares the four demo entities matching `examples/ha-states.json`
(`light.kitchen`, `sensor.hallway_temperature`, `switch.outlet_1`, `binary_sensor.foo`),
generates a long-lived access token, and writes `HA_URL` + `HA_TOKEN` to `.env.local`
on the host. The token never appears in terminal stdout. After provisioning, plain
`cargo run` (without `--fixture`) renders the same dashboard as
`cargo run --fixture examples/ha-states.json`.

### Prerequisites (in order)

1. `./run-vm.sh` — boot the QEMU/KVM dev VM. See `vm/` for setup.
2. `make vm-docker` — only required if your VM was built **before** TASK-050 landed.
   Newer VMs install Docker via cloud-init and skip this step. Idempotent.
3. `make ha-up` — provisions HA, generates the token, writes `.env.local`.

Skipping any step produces a confusing failure (SSH error, missing `docker compose`,
or empty `.env.local`). The Makefile checks VM SSH reachability before running any
`ha-*` target.

### Lifecycle commands

- `make ha-up` — provision HA Core in the VM and write the token. Atomic on success.
- `make ha-down` — stop the HA container. State and token are preserved.
- `make ha-clean` — remove the `ha_data` Docker volume and strip `HA_URL` / `HA_TOKEN`
  from `.env.local`. Stop `cargo run` first to avoid noisy disconnect logs.
- `make ha-token` — rotate the long-lived token (overwrites `HA_TOKEN` in `.env.local`).
  Use this if the existing token has been revoked.

See the `Makefile` and `ops/ha/provision-ha.sh` for the exact recipes.

### Recovery

`make ha-up` is atomic on success only. If it is interrupted mid-provision (after
HA onboarding but before the token write), the instance is **not** API-recoverable.
The recovery path is:

```sh
make ha-clean && make ha-up
```

The provisioning script detects partial state on re-run and exits with that exact
instruction.

### Verify provisioning (existence-only — never print the token)

After `make ha-up` completes, confirm `.env.local` was written without exposing the
token value:

```sh
grep -q HA_TOKEN .env.local && echo "token present"
```

`grep -q` suppresses the matching line; only the trailer prints. Avoid any verification
recipe that prints `.env.local` contents or expands `$HA_TOKEN` to your terminal — those
forms surface the secret to scrollback and shell history. Stick to existence-only checks
like the one above (or `[ -s .env.local ] && echo "env file present and non-empty"`).

### Run the app against the local instance

Source `.env.local` into the current shell with auto-export, then run cargo. This
loads `HA_URL` and `HA_TOKEN` into the process environment without echoing the values:

```sh
set -a; source .env.local; set +a
cargo run
```

Both tile labels and initial state values should match the fixture-mode output.
