"""
DBus client helper for the org.freedesktop.ratbag1 API.

Wraps raw dbus-python / dasbus calls behind a clean Python interface
so that test code never deals with DBus plumbing directly.

Requires the ``dbus-python`` package (``pip install dbus-python``).
"""

from __future__ import annotations

import dbus
from typing import Any


# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

BUS_NAME = "org.freedesktop.ratbag1"
MANAGER_PATH = "/org/freedesktop/ratbag1"
MANAGER_IFACE = "org.freedesktop.ratbag1.Manager"
DEVICE_IFACE = "org.freedesktop.ratbag1.Device"
PROFILE_IFACE = "org.freedesktop.ratbag1.Profile"
RESOLUTION_IFACE = "org.freedesktop.ratbag1.Resolution"
BUTTON_IFACE = "org.freedesktop.ratbag1.Button"
LED_IFACE = "org.freedesktop.ratbag1.Led"
PROPERTIES_IFACE = "org.freedesktop.DBus.Properties"


class RatbagDBusClient:
    """Thin wrapper around the org.freedesktop.ratbag1 DBus API."""

    def __init__(self, bus_type: str = "system"):
        if bus_type == "session":
            self._bus = dbus.SessionBus()
        else:
            self._bus = dbus.SystemBus()

    # ------------------------------------------------------------------
    # Low-level helpers
    # ------------------------------------------------------------------

    def _get_proxy(self, path: str):
        """Return a dbus proxy object for *path*.

        Introspection is disabled because the daemon's XML can contain
        tokens that dbus-python's parser chokes on (e.g. Rust type
        signatures).  All method/property calls are made by explicit
        interface name, so introspection is unnecessary.
        """
        return self._bus.get_object(BUS_NAME, path, introspect=False)

    def _get_property(self, path: str, iface: str, prop: str) -> Any:
        obj = self._get_proxy(path)
        props = dbus.Interface(obj, PROPERTIES_IFACE)
        return props.Get(iface, prop)

    def _set_property(self, path: str, iface: str, prop: str, value: Any):
        obj = self._get_proxy(path)
        # Properties.Set signature is (ssv) — the value must be a variant.
        # Because introspection is disabled, dbus-python won't auto-wrap it,
        # so we call the low-level method with an explicit signature.
        obj.get_dbus_method("Set", PROPERTIES_IFACE)(
            iface, prop, value, signature="ssv"
        )

    def _call_method(self, path: str, iface: str, method: str, *args):
        obj = self._get_proxy(path)
        iface_obj = dbus.Interface(obj, iface)
        fn = getattr(iface_obj, method)
        return fn(*args)

    # ------------------------------------------------------------------
    # Manager interface
    # ------------------------------------------------------------------

    def manager_api_version(self) -> int:
        return int(self._get_property(MANAGER_PATH, MANAGER_IFACE, "APIVersion"))

    def manager_devices(self) -> list[str]:
        paths = self._get_property(MANAGER_PATH, MANAGER_IFACE, "Devices")
        return [str(p) for p in paths]

    def load_test_device(self, json_str: str) -> str:
        """Inject a synthetic test device (requires dev-hooks)."""
        sysname = self._call_method(MANAGER_PATH, MANAGER_IFACE, "LoadTestDevice", json_str)
        return f"/org/freedesktop/ratbag1/device/{sysname}"

    def load_test_device_with_driver(
        self, driver_name: str, config_json: str, io_script_json: str
    ) -> str:
        """Inject a driver-backed test device (requires dev-hooks).

        Returns the sysname of the created device.
        """
        return str(
            self._call_method(
                MANAGER_PATH,
                MANAGER_IFACE,
                "LoadTestDeviceWithDriver",
                driver_name,
                config_json,
                io_script_json,
            )
        )

    def get_mock_io_log(self, sysname: str) -> str:
        """Return the mock I/O write log as a JSON string."""
        return str(
            self._call_method(
                MANAGER_PATH, MANAGER_IFACE, "GetMockIoLog", sysname
            )
        )

    def reset_test_device(self):
        """Remove the currently injected test device (requires dev-hooks)."""
        self._call_method(MANAGER_PATH, MANAGER_IFACE, "ResetTestDevice")

    def has_dev_hooks(self) -> bool:
        """Return True if the daemon exposes LoadTestDevice (dev-hooks build).

        Attempts a lightweight probe — calling LoadTestDevice with an
        intentionally bad payload.  If the method exists, the daemon will
        return *some* error (but not ``UnknownMethod``).  If the method
        does not exist, dbus-python raises ``UnknownMethod``.
        """
        try:
            self._call_method(
                MANAGER_PATH, MANAGER_IFACE, "LoadTestDevice", "{}"
            )
            # The call might actually succeed for an empty spec
            return True
        except dbus.exceptions.DBusException as exc:
            if "UnknownMethod" in str(exc.get_dbus_name() or ""):
                return False
            # Any other DBus error means the method exists
            return True
        except Exception:
            return False

    # ------------------------------------------------------------------
    # Device interface
    # ------------------------------------------------------------------

    def device_name(self, path: str) -> str:
        return str(self._get_property(path, DEVICE_IFACE, "Name"))

    def device_model(self, path: str) -> str:
        return str(self._get_property(path, DEVICE_IFACE, "Model"))

    def device_firmware_version(self, path: str) -> str:
        return str(self._get_property(path, DEVICE_IFACE, "FirmwareVersion"))

    def device_profiles(self, path: str) -> list[str]:
        paths = self._get_property(path, DEVICE_IFACE, "Profiles")
        return [str(p) for p in paths]

    def device_commit(self, path: str) -> int:
        return int(self._call_method(path, DEVICE_IFACE, "Commit"))

    # ------------------------------------------------------------------
    # Profile interface
    # ------------------------------------------------------------------

    def profile_index(self, path: str) -> int:
        return int(self._get_property(path, PROFILE_IFACE, "Index"))

    def profile_name(self, path: str) -> str:
        return str(self._get_property(path, PROFILE_IFACE, "Name"))

    def set_profile_name(self, path: str, name: str):
        self._set_property(path, PROFILE_IFACE, "Name", name)

    def profile_is_active(self, path: str) -> bool:
        return bool(self._get_property(path, PROFILE_IFACE, "IsActive"))

    def profile_disabled(self, path: str) -> bool:
        return bool(self._get_property(path, PROFILE_IFACE, "Disabled"))

    def set_profile_disabled(self, path: str, disabled: bool):
        self._set_property(path, PROFILE_IFACE, "Disabled", disabled)

    def profile_is_dirty(self, path: str) -> bool:
        return bool(self._get_property(path, PROFILE_IFACE, "IsDirty"))

    def profile_report_rate(self, path: str) -> int:
        return int(self._get_property(path, PROFILE_IFACE, "ReportRate"))

    def set_profile_report_rate(self, path: str, rate: int):
        self._set_property(path, PROFILE_IFACE, "ReportRate", dbus.UInt32(rate))

    def profile_report_rates(self, path: str) -> list[int]:
        rates = self._get_property(path, PROFILE_IFACE, "ReportRates")
        return [int(r) for r in rates]

    def profile_angle_snapping(self, path: str) -> int:
        return int(self._get_property(path, PROFILE_IFACE, "AngleSnapping"))

    def set_profile_angle_snapping(self, path: str, value: int):
        self._set_property(path, PROFILE_IFACE, "AngleSnapping", dbus.Int32(value))

    def profile_debounce(self, path: str) -> int:
        return int(self._get_property(path, PROFILE_IFACE, "Debounce"))

    def set_profile_debounce(self, path: str, value: int):
        self._set_property(path, PROFILE_IFACE, "Debounce", dbus.Int32(value))

    def profile_resolutions(self, path: str) -> list[str]:
        paths = self._get_property(path, PROFILE_IFACE, "Resolutions")
        return [str(p) for p in paths]

    def profile_buttons(self, path: str) -> list[str]:
        paths = self._get_property(path, PROFILE_IFACE, "Buttons")
        return [str(p) for p in paths]

    def profile_leds(self, path: str) -> list[str]:
        paths = self._get_property(path, PROFILE_IFACE, "Leds")
        return [str(p) for p in paths]

    def profile_set_active(self, path: str):
        self._call_method(path, PROFILE_IFACE, "SetActive")

    def profile_capabilities(self, path: str) -> list[int]:
        caps = self._get_property(path, PROFILE_IFACE, "Capabilities")
        return [int(c) for c in caps]

    # ------------------------------------------------------------------
    # Resolution interface
    # ------------------------------------------------------------------

    def resolution_index(self, path: str) -> int:
        return int(self._get_property(path, RESOLUTION_IFACE, "Index"))

    def resolution_is_active(self, path: str) -> bool:
        return bool(self._get_property(path, RESOLUTION_IFACE, "IsActive"))

    def resolution_is_default(self, path: str) -> bool:
        return bool(self._get_property(path, RESOLUTION_IFACE, "IsDefault"))

    def resolution_is_disabled(self, path: str) -> bool:
        return bool(self._get_property(path, RESOLUTION_IFACE, "IsDisabled"))

    def set_resolution_is_disabled(self, path: str, disabled: bool):
        self._set_property(path, RESOLUTION_IFACE, "IsDisabled", disabled)

    def resolution_value(self, path: str):
        """Return the DPI value. May be a u32 or (u32, u32) tuple."""
        return self._get_property(path, RESOLUTION_IFACE, "Resolution")

    def set_resolution_value(self, path: str, value):
        """Set the DPI value; accepts u32 or (u32, u32)."""
        if isinstance(value, (list, tuple)) and len(value) == 2:
            val = dbus.Struct(
                [dbus.UInt32(value[0]), dbus.UInt32(value[1])],
                signature="uu",
            )
        else:
            val = dbus.UInt32(int(value))
        self._set_property(path, RESOLUTION_IFACE, "Resolution", val)

    def resolution_capabilities(self, path: str) -> list[int]:
        caps = self._get_property(path, RESOLUTION_IFACE, "Capabilities")
        return [int(c) for c in caps]

    def resolution_dpi_list(self, path: str) -> list[int]:
        vals = self._get_property(path, RESOLUTION_IFACE, "Resolutions")
        return [int(v) for v in vals]

    def resolution_set_active(self, path: str):
        self._call_method(path, RESOLUTION_IFACE, "SetActive")

    def resolution_set_default(self, path: str):
        self._call_method(path, RESOLUTION_IFACE, "SetDefault")

    # ------------------------------------------------------------------
    # Button interface
    # ------------------------------------------------------------------

    def button_index(self, path: str) -> int:
        return int(self._get_property(path, BUTTON_IFACE, "Index"))

    def button_mapping(self, path: str) -> tuple:
        """Return (action_type: u32, value: variant)."""
        return self._get_property(path, BUTTON_IFACE, "Mapping")

    def set_button_mapping(self, path: str, action_type: int, value):
        """Set button mapping. value is u32 or list of (u32, u32) for macros."""
        if action_type == 4:  # Macro
            macro_val = dbus.Array(
                [dbus.Struct([dbus.UInt32(a), dbus.UInt32(b)], signature="uu") for a, b in value],
                signature="(uu)",
            )
            mapping = dbus.Struct(
                [dbus.UInt32(action_type), macro_val], signature="uv"
            )
        else:
            mapping = dbus.Struct(
                [dbus.UInt32(action_type), dbus.UInt32(int(value))], signature="uv"
            )
        self._set_property(path, BUTTON_IFACE, "Mapping", mapping)

    def button_action_types(self, path: str) -> list[int]:
        types = self._get_property(path, BUTTON_IFACE, "ActionTypes")
        return [int(t) for t in types]

    # ------------------------------------------------------------------
    # LED interface
    # ------------------------------------------------------------------

    def led_index(self, path: str) -> int:
        return int(self._get_property(path, LED_IFACE, "Index"))

    def led_mode(self, path: str) -> int:
        return int(self._get_property(path, LED_IFACE, "Mode"))

    def set_led_mode(self, path: str, mode: int):
        self._set_property(path, LED_IFACE, "Mode", dbus.UInt32(mode))

    def led_modes(self, path: str) -> list[int]:
        modes = self._get_property(path, LED_IFACE, "Modes")
        return [int(m) for m in modes]

    def led_color(self, path: str) -> tuple[int, int, int]:
        c = self._get_property(path, LED_IFACE, "Color")
        return (int(c[0]), int(c[1]), int(c[2]))

    def set_led_color(self, path: str, r: int, g: int, b: int):
        color = dbus.Struct(
            [dbus.UInt32(r), dbus.UInt32(g), dbus.UInt32(b)], signature="uuu"
        )
        self._set_property(path, LED_IFACE, "Color", color)

    def led_secondary_color(self, path: str) -> tuple[int, int, int]:
        c = self._get_property(path, LED_IFACE, "SecondaryColor")
        return (int(c[0]), int(c[1]), int(c[2]))

    def set_led_secondary_color(self, path: str, r: int, g: int, b: int):
        color = dbus.Struct(
            [dbus.UInt32(r), dbus.UInt32(g), dbus.UInt32(b)], signature="uuu"
        )
        self._set_property(path, LED_IFACE, "SecondaryColor", color)

    def led_tertiary_color(self, path: str) -> tuple[int, int, int]:
        c = self._get_property(path, LED_IFACE, "TertiaryColor")
        return (int(c[0]), int(c[1]), int(c[2]))

    def set_led_tertiary_color(self, path: str, r: int, g: int, b: int):
        color = dbus.Struct(
            [dbus.UInt32(r), dbus.UInt32(g), dbus.UInt32(b)], signature="uuu"
        )
        self._set_property(path, LED_IFACE, "TertiaryColor", color)

    def led_color_depth(self, path: str) -> int:
        return int(self._get_property(path, LED_IFACE, "ColorDepth"))

    def led_brightness(self, path: str) -> int:
        return int(self._get_property(path, LED_IFACE, "Brightness"))

    def set_led_brightness(self, path: str, brightness: int):
        self._set_property(path, LED_IFACE, "Brightness", dbus.UInt32(brightness))

    def led_effect_duration(self, path: str) -> int:
        return int(self._get_property(path, LED_IFACE, "EffectDuration"))

    def set_led_effect_duration(self, path: str, duration: int):
        self._set_property(
            path, LED_IFACE, "EffectDuration", dbus.UInt32(duration)
        )
