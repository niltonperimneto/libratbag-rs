libratbag
=========

<img src="https://libratbag.github.io/_images/logo.svg" alt="" width="30%" align="right">

libratbag provides **ratbagd**, a DBus daemon to configure input devices,
mainly gaming mice. The daemon provides a generic way to access the various
features exposed by these mice and abstracts away hardware-specific and
kernel-specific quirks.

**As of version 2.0, the ratbagd daemon has been rewritten in Rust
(`ratbagd-rs`) and migrated to an unprivileged session daemon.** The new daemon speaks the same `org.freedesktop.ratbag1`
DBus API (version 2) and uses the same device database and `.device` files.
The old C daemon has been removed and the CLI replaced with a new Rust
`ratbagctl` tool built on the same DBus API.

> ⚠️ **Breaking change: ratbagd now runs on the session bus, not the system
> bus.** This intentionally breaks compatibility with current
> [Piper](https://github.com/libratbag/piper/) releases, which connect to
> ratbagd on the **system** bus. See
> [Session Bus Migration](#session-bus-migration-breaking-change-for-piper)
> below for what changed and why.

Session Bus Migration (Breaking Change for Piper)
-------------------------------------------------

`ratbagd-rs` no longer runs as a `root` system daemon on the **system bus**.
It now runs as an unprivileged **session daemon** on the user's **session
bus** (`zbus::Connection::session()`), spawned and managed by
`systemd --user`. Device access is granted to the physically seated user via
`udev` + `uaccess` instead of by running as root.

### What this breaks

Existing [Piper](https://github.com/libratbag/piper/) releases connect to
ratbagd over the **system bus** (`Gio.BusType.SYSTEM`). Because the daemon no
longer claims `org.freedesktop.ratbag1` on the system bus, **stock Piper can
no longer find or talk to ratbagd** and will report that the daemon is not
running. Piper would need to be patched to connect to the session bus to work
with this daemon. [Twister](#twister-desktop-gui), the GUI included in this
repository, already connects on the session bus.

The new `data/60-ratbagd.rules` udev rule (which tags raw HID interfaces with
`uaccess`) does **not** itself affect Piper — it only governs `/dev/hidraw*`
node permissions. The compatibility break comes from the bus migration, not
the udev rule.

### Why we broke compatibility

The legacy architecture ran `ratbagd` as `root` solely to bypass file
permissions on `/dev/hidraw*`. This violates the principle of least
privilege: configuring DPI, RGB, or button bindings does not warrant
administrative access. USB hardware is untrusted, and a malicious or
compromised device can send malformed HID reports crafted to exploit parsing
flaws — and any such exploit in a root daemon becomes a full system
compromise (privilege escalation).

Running as an unprivileged session daemon **contains the blast radius**: an
exploit triggered by a malicious mouse is confined to the unprivileged user's
session, leaving the host OS intact. It also models device settings correctly
as per-user preferences and behaves sensibly on multi-user systems, where the
kernel's seat management (`systemd-logind`) grants hardware access only to the
physically seated user.

See [SESSION_DAEMON_PLAN.md](SESSION_DAEMON_PLAN.md) for the full
architectural rationale and roadmap.

Supported Devices
-----------------

libratbag supports devices from Asus, Etekcity, GSkill, Logitech (HID++ 1.0
and 2.0, G300, G600), MarsGaming, OpenInput, Roccat (including Kone Pure /
Kone EMP variants), Sinowealth (including Nubwo), and Steelseries.

See [the device files](https://github.com/niltonperimneto/libratbag-rs/tree/master/data/devices)
for a complete list of supported devices.

Users interact through a GUI like
[Twister](twister/) (a modern Tauri + Svelte desktop app included in this
repository), or the `ratbagctl` command-line tool (see below).
[Piper](https://github.com/libratbag/piper/) is **not currently compatible**
because the daemon moved to the session bus — see
[Session Bus Migration](#session-bus-migration-breaking-change-for-piper).

What Changed in the Rust Rewrite
---------------------------------

The core `ratbagd` daemon has been rewritten from C to Rust. Key changes:

- **Async, actor-based architecture** — each connected device gets its own
  Tokio task (actor) that owns the HID file descriptor and serializes all
  hardware I/O through an `mpsc` channel. DBus interface objects share
  device state via `Arc<RwLock<DeviceInfo>>`.
- **Structured driver framework** — all drivers implement a common
  `DeviceDriver` trait (`probe`, `load_profiles`, `commit`). Hardware I/O is
  abstracted behind `DeviceIo` (async hidraw read/write, feature report
  ioctls, request/response matching with timeouts and retries).
- **Full driver parity** — all 15 drivers from the C codebase have been
  ported: `asus`, `etekcity`, `gskill`, `hidpp10`, `hidpp20`,
  `logitech_g300`, `logitech_g600`, `marsgaming`, `openinput`, `roccat`
  (with Kone Pure / Kone EMP), `sinowealth`, `sinowealth_nubwo`, and
  `steelseries`.
- **Dev-hooks feature** — compile with `--features dev-hooks` to enable
  `LoadTestDevice` / `ResetTestDevice` DBus methods on the Manager
  interface, allowing integration tests to inject synthetic devices without
  real hardware.
- **Session daemon, not a root system daemon** — the daemon now runs
  unprivileged on the user's session bus, managed by `systemd --user`, with
  device access delegated via `udev` + `uaccess`. The legacy system-bus DBus
  policy and `User=root` activation are no longer used. See
  [Session Bus Migration](#session-bus-migration-breaking-change-for-piper).
- **License change** — the Rust daemon (`ratbagd-rs/`) is licensed under
  **GPLv3**. Supporting assets (service templates, device data, docs) remain
  under MIT/Expat (see the License section below).

### What stays the same

- The `org.freedesktop.ratbag1` DBus API (version 2) — all interfaces
  (`Manager`, `Device`, `Profile`, `Resolution`, `Button`, `LED`) are
  wire-compatible with the C daemon (but now served on the **session** bus;
  see [Session Bus Migration](#session-bus-migration-breaking-change-for-piper)).
- The `.device` file database in `data/devices/`.

Installing libratbag-rs from system packages
---------------------------------------------

libratbag-rs is not yet packaged for distributions. See the
[Compiling](#compiling-libratbag) section below to build from source.

Build Requirements
------------------

- **Rust toolchain** — a stable Rust compiler (Rust 1.85+; edition 2024 for
  `ratbagd-rs`, edition 2021 for `ratbagctl-rs`) and Cargo.
  Install via [rustup](https://rustup.rs/) or your distribution's package
  manager.
- **Meson** (>= 0.59) and **Ninja**.
- **System libraries**: `libudev` (required for runtime udev monitoring) and
  `systemd` (only for installing the unit file; optional if you package the
  service files yourself).
- **pkg-config** — used by Meson to locate `libudev` and `systemd`.

The Rust daemon itself depends on `tokio`, `zbus`, `nix`, `udev`, `serde`,
`tracing`, and other crates — Cargo resolves these automatically. The CLI
tool (`ratbagctl-rs/`) depends on `clap`, `zbus`, `tokio`, and `anyhow`.
`Cargo.lock` files are committed for reproducible builds
(`cargo build --locked`).

Compiling libratbag
-------------------

libratbag uses the [meson build system](http://mesonbuild.com) which in
turn uses Ninja to invoke the compilers. Meson drives the Rust build
automatically via Cargo. Run the following commands to clone libratbag and
build everything:

    git clone https://github.com/niltonperimneto/libratbag-rs.git
    cd libratbag-rs
    meson setup builddir --prefix=/usr
    meson compile -C builddir
    sudo meson install -C builddir

To build or re-build after code changes:

    meson compile -C builddir
    sudo meson install -C builddir

To remove/uninstall:

    sudo ninja -C builddir uninstall

Note: `builddir` is the build output directory and can be changed to any
other directory name.

### Configure-time options

To list all options:

    meson configure builddir

Notable options:

| Option | Default | Description |
|---|---|---|
| `-Dsystemd=true` | `true` | Install the systemd unit file |
| `-Dsystemd-unit-dir=PATH` | auto | Override the systemd unit directory |
| `-Ddbus-root-dir=PATH` | auto | Override the DBus configuration directory |
| `-Ddbus-group=GROUP` | (everyone) | Restrict DBus access to a UNIX group |

### Building with dev-hooks (for testing)

To enable the synthetic test device DBus methods, edit the Cargo build
flags in `meson.build` or build the Rust crate directly:

    cd ratbagd-rs
    cargo build --release --features dev-hooks

**Never enable `dev-hooks` in production builds.**

Running ratbagd as DBus-activated systemd service
-------------------------------------------------

ratbagd is intended to run as a DBus-activated systemd service. At install
time, the following files are placed on the system:

| File | Purpose |
|---|---|
| `/usr/share/dbus-1/system.d/org.freedesktop.ratbag1.conf` | DBus policy (who can own/talk to the bus name) |
| `/usr/share/dbus-1/system-services/org.freedesktop.ratbag1.service` | DBus activation (tells the bus how to start the daemon) |
| `$unitdir/ratbagd.service` | systemd unit (`Type=dbus`, `BusName=org.freedesktop.ratbag1`) |

Both the DBus activation file and the systemd unit point `Exec`/`ExecStart`
at `$sbindir/ratbagd` — the installed Rust binary.

See also the configure-time options `-Dsystemd-unit-dir` and
`-Ddbus-root-dir`. Developers are encouraged to symlink to the files in the
git repository.

### Activating the service

After installing, reload the service manager:

    sudo systemctl daemon-reload
    sudo systemctl reload dbus.service

Enable the service (for automatic DBus activation):

    sudo systemctl enable ratbagd.service

From now on, any DBus access to `org.freedesktop.ratbag1` (for example via
`busctl introspect org.freedesktop.ratbag1 /org/freedesktop/ratbag1`) will
automatically start the Rust daemon through DBus activation.

### Verifying the Rust daemon is running

    systemctl status ratbagd
    journalctl -u ratbagd -n 20   # should show "Starting ratbagd-rs version ..."

You can also start it directly for debugging:

    sudo ratbagd                             # production
    sudo RUST_LOG=debug ratbagd              # verbose logging via tracing

Using ratbagctl
---------------

`ratbagctl` is the command-line interface for configuring devices. It talks
to the running `ratbagd` daemon over DBus.

### Quick examples

    ratbagctl list                              # list connected devices
    ratbagctl info 0                            # show device details
    ratbagctl commit 0                          # commit pending changes to hardware
    ratbagctl profile list 0                    # list profiles for device 0
    ratbagctl profile info 0 0                  # show profile 0 details
    ratbagctl profile active 0 1                # switch to profile 1
    ratbagctl profile name 0 0 "Gaming"         # set profile name
    ratbagctl profile enable 0 1                # enable profile 1
    ratbagctl profile angle-snapping 0 0 on     # enable angle snapping
    ratbagctl profile debounce 0 0 10           # set debounce to 10 ms
    ratbagctl resolution dpi 0 0 0 800          # set resolution 0 to 800 DPI
    ratbagctl resolution active 0 0 2           # activate resolution 2
    ratbagctl resolution default 0 0 1          # set default resolution to 1
    ratbagctl button list 0 0                   # list button mappings
    ratbagctl button set-button 0 0 1 3         # set button 1 to logical button 3
    ratbagctl button set-key 0 0 1 30           # set button 1 to keycode 30 (KEY_A)
    ratbagctl button set-macro 0 0 1 30:1 30:0  # set button 1 to a key macro
    ratbagctl led mode 0 0 0 breathing          # set LED 0 to breathing mode
    ratbagctl led color 0 0 0 ff0000            # set LED color to red
    ratbagctl led secondary-color 0 0 0 00ff00  # set secondary LED color
    ratbagctl led brightness 0 0 0 200          # set brightness to 200
    ratbagctl led duration 0 0 0 1000           # set effect duration to 1000 ms

### Subcommands

| Command | Description |
|---|---|
| **General** | |
| `list` | List all connected devices (shows API version) |
| `info <device>` | Show detailed info for a device |
| `commit <device>` | Commit all pending changes to hardware |
| **Profile** | |
| `profile list <device>` | List profiles (name, rate, dirty state) |
| `profile info <device> <profile>` | Show full profile details |
| `profile active <device> <profile>` | Set the active profile |
| `profile name <device> <profile> [name]` | Get or set profile name |
| `profile enable <device> <profile>` | Enable a profile |
| `profile disable <device> <profile>` | Disable a profile |
| `profile rate <device> <profile> <hz>` | Set profile report rate |
| `profile angle-snapping <device> <profile> [on\|off]` | Get or set angle snapping |
| `profile debounce <device> <profile> [ms]` | Get or set debounce time |
| **Resolution** | |
| `resolution list <device> <profile>` | List resolutions (DPI list, capabilities) |
| `resolution dpi <device> <profile> <res> [dpi]` | Get or set DPI |
| `resolution active <device> <profile> <res>` | Set active resolution |
| `resolution default <device> <profile> <res>` | Set default resolution |
| `resolution enable <device> <profile> <res>` | Enable a resolution slot |
| `resolution disable <device> <profile> <res>` | Disable a resolution slot |
| **Button** | |
| `button list <device> <profile>` | List buttons |
| `button get <device> <profile> <button>` | Get button mapping details |
| `button set-button <device> <profile> <btn> <value>` | Map to logical button (action type 1) |
| `button set-special <device> <profile> <btn> <value>` | Map to special action (action type 2) |
| `button set-key <device> <profile> <btn> <keycode>` | Map to key (action type 3) |
| `button set-macro <device> <profile> <btn> <events...>` | Map to macro (action type 4); events are `keycode:direction` pairs |
| `button disable <device> <profile> <button>` | Disable a button |
| **LED** | |
| `led list <device> <profile>` | List LEDs |
| `led get <device> <profile> <led>` | Get LED info (mode, colors, brightness, duration, color depth) |
| `led mode <device> <profile> <led> <mode>` | Set mode (off, solid, cycle, wave, starlight, breathing, tricolor) |
| `led color <device> <profile> <led> <hex>` | Set primary color (e.g. `ff0000`) |
| `led secondary-color <device> <profile> <led> <hex>` | Set secondary color |
| `led tertiary-color <device> <profile> <led> <hex>` | Set tertiary color |
| `led brightness <device> <profile> <led> <0-255>` | Set brightness |
| `led duration <device> <profile> <led> <ms>` | Set effect duration in milliseconds |
| **Test / Dev** | |
| `test load-device <json_file>` | Load a test device from a JSON file |
| `test reset` | Remove all test devices |

`<device>` can be a zero-based index from `ratbagctl list` or a sysname
substring. All write commands automatically commit changes to hardware.

Twister (Desktop GUI)
---------------------

Twister is a modern, desktop-agnostic graphical frontend for configuring
gaming mice. It is built with Tauri 2 and Svelte 5 and is included in this
repository under `twister/`.

Twister communicates with `ratbagd` over the same `org.freedesktop.ratbag1`
DBus interface, so it works as a drop-in replacement for Piper on any Linux
desktop environment.

**Status:** Early alpha — core features (DPI, buttons, LEDs, profiles) work.

See [twister/README.md](twister/README.md) for build instructions,
screenshots, and detailed documentation.

Testing
-------

The `test/` directory contains a Python integration test suite that exercises
the full `org.freedesktop.ratbag1` DBus API against the Rust daemon built
with the `dev-hooks` feature. Tests use `pytest` and cover the Manager,
Device, Profile, Resolution, Button, and LED interfaces.

See [test/README.md](test/README.md) for prerequisites and usage.

The DBus Interface
-------------------

Full documentation of the DBus interface to interact with devices is
available here: [ratbagd DBus Interface description](https://libratbag.github.io/).

The daemon exposes the following interfaces on the session bus under
`org.freedesktop.ratbag1`:

| Interface | Object Path | Description |
|---|---|---|
| `Manager` | `/org/freedesktop/ratbag1` | Entry point; lists connected devices |
| `Device` | `/org/freedesktop/ratbag1/device/<sysname>` | Per-device (name, model, profiles list) |
| `Profile` | `.../p<N>` | Per-profile (active profile, DPI list) |
| `Resolution` | `.../p<N>/r<N>` | Per-resolution (DPI x/y, report rate) |
| `Button` | `.../p<N>/b<N>` | Per-button (action type, mapping) |
| `LED` | `.../p<N>/l<N>` | Per-LED (mode, color, brightness, effect rate) |

Architecture
------------

### High-level data flow

    +---------+
    | Twister |--+
    +---------+  |   +------+    +-------------------+
                 +-> | DBus | -> | ratbagd-rs (Rust) | -> /dev/hidraw*
    +---------+  |   +------+    +-------------------+
    |  Piper  |--+                      |
    +---------+               +------+------+
                              | Device Actor | (one per mouse, owns DeviceIo)
                              +------+------+
                                     |
                              +------+------+
                              |   Driver    | (HID++, Roccat, Steelseries, …)
                              +-------------+

### Internal Rust architecture

- **`main.rs`** — entry point; initializes tracing, loads the device
  database, spawns the udev monitor, and starts the DBus server.
- **`dbus/`** — zbus interface implementations for `Manager`, `Device`,
  `Profile`, `Resolution`, `Button`, and `LED`.
- **`actor.rs`** — per-device actor task that serializes hardware I/O.
  DBus handlers send `ActorCommand` messages; the actor executes them
  against the `DeviceDriver` + `DeviceIo`.
- **`driver/`** — the `DeviceDriver` trait and all protocol implementations.
  `DeviceIo` wraps async hidraw I/O with feature report ioctl support.
- **`device.rs`** — `DeviceInfo` and its children (`ProfileInfo`,
  `ResolutionInfo`, `ButtonInfo`, `LedInfo`) — the canonical device state
  shared between DBus objects and the actor via `Arc<RwLock<…>>`.
- **`device_database.rs`** — parser for `.device` files (INI-like config).
- **`udev_monitor.rs`** — monitors hidraw device add/remove events and
  sends `DeviceAction` messages to the main event loop.

Adding Devices to libratbag
---------------------------

libratbag relies on a device database to match a device with its driver.
See the [data/devices/](https://github.com/niltonperimneto/libratbag-rs/tree/master/data/devices)
directory for the set of known devices. These files are usually installed
into `$prefix/$datadir` (e.g. `/usr/share/libratbag/`).

Adding a new device can be as simple as adding a new `.device` file. This is
the case for many devices with a shared protocol (e.g. Logitech's HID++).
See the
[data/devices/device.example](https://github.com/niltonperimneto/libratbag-rs/tree/master/data/devices/device.example)
file for guidance on what information must be set. Look for existing devices
from the same vendor as guidance too.

If the device has a different protocol and doesn't work after adding the
device file, you'll have to start reverse-engineering the device-specific
protocol. Good luck :)

Source
------

    git clone https://github.com/niltonperimneto/libratbag-rs.git

Bugs
----

Bugs can be reported in [our issue tracker](https://github.com/niltonperimneto/libratbag-rs/issues)

Discussions
-----------

For questions, feature requests, or general discussion, please open an
[issue](https://github.com/niltonperimneto/libratbag-rs/issues) on GitHub.

Device-specific notes
---------------------

A number of device-specific notes and observations can be found in the
upstream project wiki:
https://github.com/libratbag/libratbag/wiki/Devices

License
-------

This project uses a **dual-license** structure:

- **ratbagd-rs** (the Rust daemon in `ratbagd-rs/`) is licensed under the
  **GNU General Public License v3.0 (GPLv3)**.
- **ratbagctl-rs** (the CLI tool in `ratbagctl-rs/`) is licensed under the
  **GNU General Public License v3.0 or later (GPL-3.0-or-later)**.
- **Twister** (the desktop GUI in `twister/`) is licensed under the
  **GNU General Public License v3.0 or later (GPL-3.0-or-later)**.
- **Supporting assets** (service templates, device data, documentation, and
  other non-daemon content) remain licensed under the **MIT/Expat** license.

> Permission is hereby granted, free of charge, to any person obtaining a
> copy of this software and associated documentation files (the "Software"),
> to deal in the Software without restriction, including without limitation
> the rights to use, copy, modify, merge, publish, distribute, sublicense,
> and/or sell copies of the Software, and to permit persons to whom the
> Software is furnished to do so, subject to the following conditions: [...]

See the [COPYING](COPYING) file for the MIT license and
`ratbagd-rs/Cargo.toml` for the GPLv3 declaration.
