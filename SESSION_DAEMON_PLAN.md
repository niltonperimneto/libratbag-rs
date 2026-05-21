# Architecture Plan: A Modern, Session-Level Successor to libratbag

This document outlines the architectural plan for transitioning the `libratbag` ecosystem from a monolithic system daemon to a modern, unprivileged session daemon, mitigating severe security vectors while improving the user experience on multi-user systems.

## 1. The Fundamental Flaw of the Legacy Architecture

Historically, the `ratbagd` daemon runs as `root` (a system-wide daemon). It does this to easily bypass file permissions, as `root` can read and write to any `/dev/hidrawX` device on the system. While this simplifies installation, it violates the **Principle of Least Privilege**. Configuring RGB colors or DPI stages on a gaming mouse does not require administrative privileges—such as the ability to wipe hard drives or install kernel modules.

### The Threat Vector: Untrusted USB Input
USB hardware is inherently untrusted. A malicious or compromised USB device can send malformed HID reports designed to exploit buffer overflows or parsing logic flaws. Because the legacy `ratbagd` parser runs as `root`, any exploit triggered during the parsing of these binary HID reports immediately grants the attacker full system control. This constitutes a severe privilege escalation vulnerability.

## 2. The Successor Architecture

To neutralize this threat vector, the successor architecture will model itself after modern hardware daemons (e.g., `clackd`), enforcing strict session isolation.

### 2.1. Unprivileged Session Daemon
The daemon will run entirely as the standard user, bound to the user's **Session Bus** instead of the System Bus.
* **Blast Radius Containment:** If the daemon crashes or is exploited by a maliciously crafted mouse (e.g., a Rubber Ducky or compromised firmware), the damage is strictly contained to the unprivileged user's session files. The host operating system remains secure.
* **User-Specific Context:** Gaming mouse configurations (DPI stages, polling rates, macro bindings, and RGB profiles) are inherently user preferences. A session daemon naturally handles multi-user environments. For example, if Alice alters the DPI via SSH in the background, her connection will be rejected by the kernel, preventing her from interfering with Bob who is actively playing a game.

### 2.2. udev + uaccess Authorization
Instead of running as root to brute-force device access, the installation package will provide `udev` rules that match known gaming mice and tag them with `uaccess`.
* **Dynamic Scoping:** The `uaccess` tag securely tells the OS to grant access automatically.
* **No Root Required:** The daemon simply opens the device node, naturally authorized by the OS, with zero privilege escalation.

### 2.3. Hardware Delegation via systemd-logind
The Linux kernel (via `systemd-logind`) securely delegates read/write permission of the mouse's `/dev/hidrawX` node exclusively to the physically seated user.
* When a user logs in and sits at the physical monitor, the kernel grants their session daemon permission to configure the mouse. Background access is inherently denied by the kernel's seat management.

## 3. Implementation Roadmap

The transition to an unprivileged session daemon will be executed in the following phases:

### Phase 1: Bus and Service Migration
1. **Migrate to Session Bus:** Refactor `ratbagd-rs` and `ratbagctl-rs` to default to `zbus::Connection::session()` instead of `system()`.
2. **User Systemd Service:** Move the activation unit from `/usr/lib/systemd/system/ratbagd.service` to `/usr/lib/systemd/user/ratbagd.service`. The daemon will now be spawned and managed by `systemd --user`.
3. **Deprecate Security Policies:** Completely remove the `/etc/dbus-1/system.d/org.freedesktop.ratbag1.conf` file, as Polkit and D-Bus system policies are no longer required for session services.

### Phase 2: Hardware Access Delegation
1. **Udev Rule Generation:** Update the device database build step to generate `udev` rules that apply `TAG+="uaccess"` to all known devices (matching by `idVendor` and `idProduct` or device usage pages).
2. **Rule Deployment:** Ensure the installation process drops these rules into `/usr/lib/udev/rules.d/` or `/etc/udev/rules.d/`.

### Phase 3: State and File Isolation
1. **User-Local Storage:** Migrate persistent device configurations and profiles from global paths (e.g., `/var/lib/ratbag`) to the XDG Base Directory specification (e.g., `~/.config/libratbag/` or `~/.local/state/libratbag/`).
2. **Daemon Hardening:** Implement systemd user service hardening features, stripping away all unnecessary capabilities.

## 4. Engineering Standards for Implementation

As part of the migration to the new Rust-based architecture, the implementation must adhere to strict horizontal auditability standards:

1. **Concurrency Control:** All shared state across DBus handlers and async device actors must be deterministically mediated via `Arc<Mutex<T>>` or `RwLock<T>`. Global mutable variables are strictly prohibited.
2. **Deterministic State Mutation:** In-place modification of device state structures must be replaced with purely functional, immutable state transitions that return novel state structures.
3. **Type-Safe Failure Resolution:** The usage of thread-panicking macros (`unwrap()`, `expect()`, `panic!`) is strictly forbidden. All parsing logic for HID reports must utilize comprehensive `Result<T, E>` chains with domain-specific errors, ensuring safe degradation upon encountering malicious input.
4. **Minimalist Dependency Management:** The daemon must maintain a minimal dependency footprint, utilizing the Rust Standard Library as thoroughly as possible. Necessary external crates must employ strict cryptographic hash pinning to prevent the introduction of compromised code.

Building a session-level, unprivileged daemon that relies on `udev` rules is unequivocally the safest and most architecturally sound way to build hardware configuration tools on modern Linux!
