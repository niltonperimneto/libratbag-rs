"""
Tests for the org.freedesktop.ratbag1.Manager and Device DBus interfaces.

Validates API version, device enumeration, and basic device properties
using synthetic test devices injected through the dev-hooks feature.
"""

import time

import pytest

from .conftest import (
    MINIMAL_DEVICE_JSON,
    SIMPLE_DEVICE_JSON,
    MULTI_PROFILE_DEVICE_JSON,
)
from .ratbag_dbus import RatbagDBusClient


# ---------------------------------------------------------------------------
# Helper: load a device and return its path
# ---------------------------------------------------------------------------


def _load_and_get_device(client: RatbagDBusClient, json_str: str) -> str:
    """Load a test device and return its object path. Retries briefly."""
    expected_path = client.load_test_device(json_str)
    # Give the daemon a moment to register the device on DBus
    deadline = time.monotonic() + 3.0
    while time.monotonic() < deadline:
        devices = client.manager_devices()
        if expected_path in devices:
            return expected_path
        time.sleep(0.1)
    pytest.fail(f"Test device {expected_path} did not appear in Devices list within timeout")


# ===========================================================================
# Manager tests
# ===========================================================================


class TestManager:
    """org.freedesktop.ratbag1.Manager interface tests."""

    def test_api_version(self, dbus_client: RatbagDBusClient):
        """APIVersion must be 2 for the current protocol."""
        assert dbus_client.manager_api_version() == 2

    def test_devices_initially_present(self, dbus_client: RatbagDBusClient):
        """Devices list should be accessible (may be empty without hardware)."""
        devices = dbus_client.manager_devices()
        assert isinstance(devices, list)

    @pytest.mark.requires_dev_hooks
    def test_load_minimal_test_device(self, dbus_client: RatbagDBusClient):
        """Loading an empty JSON spec should produce a valid device."""
        path = _load_and_get_device(dbus_client, MINIMAL_DEVICE_JSON)
        assert path.startswith("/org/freedesktop/ratbag1/")

    @pytest.mark.requires_dev_hooks
    def test_load_test_device_appears_in_devices(
        self, dbus_client: RatbagDBusClient
    ):
        """After loading a test device, it should appear in the Devices list."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        devices = dbus_client.manager_devices()
        assert path in devices

    @pytest.mark.requires_dev_hooks
    def test_reset_test_device(self, dbus_client: RatbagDBusClient):
        """ResetTestDevice should remove the injected device."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        dbus_client.reset_test_device()
        # Allow a moment for removal to propagate
        time.sleep(0.5)
        devices = dbus_client.manager_devices()
        assert path not in devices

    @pytest.mark.requires_dev_hooks
    def test_load_replaces_previous(self, dbus_client: RatbagDBusClient):
        """Loading a new test device should replace the previous one."""
        path1 = _load_and_get_device(dbus_client, MINIMAL_DEVICE_JSON)
        path2 = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        devices = dbus_client.manager_devices()
        assert path1 not in devices
        assert path2 in devices


# ===========================================================================
# Device tests
# ===========================================================================


@pytest.mark.requires_dev_hooks
class TestDevice:
    """org.freedesktop.ratbag1.Device interface tests."""

    def test_device_name(self, dbus_client: RatbagDBusClient):
        """Test device should have a recognisable name."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        name = dbus_client.device_name(path)
        assert "Test Device" in name

    def test_device_model(self, dbus_client: RatbagDBusClient):
        """Test devices use model string 'test:0000:0000:0'."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        model = dbus_client.device_model(path)
        assert model == "test:0000:0000:0"

    def test_firmware_version_is_string(self, dbus_client: RatbagDBusClient):
        """FirmwareVersion should be a string (may be empty for test devices)."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        fw = dbus_client.device_firmware_version(path)
        assert isinstance(fw, str)

    def test_profiles_list_populated(self, dbus_client: RatbagDBusClient):
        """Device should expose the right number of profile paths."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profiles = dbus_client.device_profiles(path)
        assert len(profiles) == 1

    def test_multi_profile_count(self, dbus_client: RatbagDBusClient):
        """Multi-profile device should expose all profiles."""
        path = _load_and_get_device(dbus_client, MULTI_PROFILE_DEVICE_JSON)
        profiles = dbus_client.device_profiles(path)
        assert len(profiles) == 3

    def test_profile_paths_are_children(self, dbus_client: RatbagDBusClient):
        """Profile paths should be children of the device path."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profiles = dbus_client.device_profiles(path)
        for profile_path in profiles:
            assert profile_path.startswith(path + "/p")

    def test_commit_test_device(self, dbus_client: RatbagDBusClient):
        """Commit on a test device (no actual hardware) should return status."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        result = dbus_client.device_commit(path)
        # Test devices have no actor, so commit returns 1 (no driver)
        assert isinstance(result, int)
