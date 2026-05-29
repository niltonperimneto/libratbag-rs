"""
Tests for the org.freedesktop.ratbag1.Profile and Resolution DBus interfaces.

Validates profile properties (active, disabled, rate, angle_snapping, debounce),
child object enumeration, and resolution read/write (unified & separate DPI).
"""

import time

import pytest

from .conftest import (
    SIMPLE_DEVICE_JSON,
    MULTI_PROFILE_DEVICE_JSON,
    SEPARATE_DPI_DEVICE_JSON,
)
from .ratbag_dbus import RatbagDBusClient

pytestmark = pytest.mark.requires_dev_hooks


# ---------------------------------------------------------------------------
# Helper
# ---------------------------------------------------------------------------


def _load_and_get_device(client: RatbagDBusClient, json_str: str) -> str:
    expected_path = client.load_test_device(json_str)
    deadline = time.monotonic() + 3.0
    while time.monotonic() < deadline:
        devices = client.manager_devices()
        if expected_path in devices:
            return expected_path
        time.sleep(0.1)
    pytest.fail(f"Test device {expected_path} did not appear within timeout")


def _first_profile(client: RatbagDBusClient, device_path: str) -> str:
    profiles = client.device_profiles(device_path)
    assert profiles, "Device has no profiles"
    return profiles[0]


# ===========================================================================
# Profile tests
# ===========================================================================


class TestProfile:
    """org.freedesktop.ratbag1.Profile interface tests."""

    def test_profile_index(self, dbus_client: RatbagDBusClient):
        """Profile index should match its position."""
        path = _load_and_get_device(dbus_client, MULTI_PROFILE_DEVICE_JSON)
        profiles = dbus_client.device_profiles(path)
        for i, profile_path in enumerate(profiles):
            assert dbus_client.profile_index(profile_path) == i

    def test_profile_is_active(self, dbus_client: RatbagDBusClient):
        """First profile should be active in the multi-profile spec."""
        path = _load_and_get_device(dbus_client, MULTI_PROFILE_DEVICE_JSON)
        profiles = dbus_client.device_profiles(path)
        assert dbus_client.profile_is_active(profiles[0]) is True
        assert dbus_client.profile_is_active(profiles[1]) is False

    def test_profile_disabled(self, dbus_client: RatbagDBusClient):
        """Third profile in multi-profile spec should be disabled."""
        path = _load_and_get_device(dbus_client, MULTI_PROFILE_DEVICE_JSON)
        profiles = dbus_client.device_profiles(path)
        assert dbus_client.profile_disabled(profiles[0]) is False
        assert dbus_client.profile_disabled(profiles[2]) is True

    def test_set_profile_disabled(self, dbus_client: RatbagDBusClient):
        """Toggling Disabled on a profile should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        assert dbus_client.profile_disabled(profile) is False
        dbus_client.set_profile_disabled(profile, True)
        assert dbus_client.profile_disabled(profile) is True
        dbus_client.set_profile_disabled(profile, False)
        assert dbus_client.profile_disabled(profile) is False

    def test_profile_name_read_write(self, dbus_client: RatbagDBusClient):
        """Setting and reading a profile name should round-trip."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        dbus_client.set_profile_name(profile, "Gaming")
        assert dbus_client.profile_name(profile) == "Gaming"

    def test_profile_report_rate(self, dbus_client: RatbagDBusClient):
        """Report rate should match the spec."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        assert dbus_client.profile_report_rate(profile) == 1000

    def test_set_profile_report_rate(self, dbus_client: RatbagDBusClient):
        """Modifying the report rate should be reflected."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        dbus_client.set_profile_report_rate(profile, 500)
        assert dbus_client.profile_report_rate(profile) == 500

    def test_profile_report_rates(self, dbus_client: RatbagDBusClient):
        """Supported report rates list should match the spec."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        rates = dbus_client.profile_report_rates(profile)
        assert 125 in rates
        assert 1000 in rates

    def test_profile_angle_snapping_default(self, dbus_client: RatbagDBusClient):
        """Angle snapping should default to -1 (unsupported) for test devices."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        assert dbus_client.profile_angle_snapping(profile) == -1

    def test_set_profile_angle_snapping(self, dbus_client: RatbagDBusClient):
        """Setting angle snapping should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        dbus_client.set_profile_angle_snapping(profile, 1)
        assert dbus_client.profile_angle_snapping(profile) == 1

    def test_profile_debounce_default(self, dbus_client: RatbagDBusClient):
        """Debounce should default to -1 (unsupported) for test devices."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        assert dbus_client.profile_debounce(profile) == -1

    def test_profile_is_dirty_after_mutation(self, dbus_client: RatbagDBusClient):
        """IsDirty should become true after mutating a property."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        # Initially clean for fresh test devices
        dbus_client.set_profile_report_rate(profile, 250)
        assert dbus_client.profile_is_dirty(profile) is True

    def test_profile_capabilities_list(self, dbus_client: RatbagDBusClient):
        """Capabilities should be a (possibly empty) list of u32 values."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        caps = dbus_client.profile_capabilities(profile)
        assert isinstance(caps, list)

    def test_report_rate_clamped_low(self, dbus_client: RatbagDBusClient):
        """Report rate below 125 should be clamped to 125."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        dbus_client.set_profile_report_rate(profile, 50)
        assert dbus_client.profile_report_rate(profile) == 125

    def test_report_rate_clamped_high(self, dbus_client: RatbagDBusClient):
        """Report rate above 8000 should be clamped to 8000."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        dbus_client.set_profile_report_rate(profile, 99999)
        assert dbus_client.profile_report_rate(profile) == 8000

    def test_set_active_profile(self, dbus_client: RatbagDBusClient):
        """SetActive should switch the active profile."""
        path = _load_and_get_device(dbus_client, MULTI_PROFILE_DEVICE_JSON)
        profiles = dbus_client.device_profiles(path)
        assert dbus_client.profile_is_active(profiles[0]) is True
        assert dbus_client.profile_is_active(profiles[1]) is False

        dbus_client.profile_set_active(profiles[1])
        assert dbus_client.profile_is_active(profiles[0]) is False
        assert dbus_client.profile_is_active(profiles[1]) is True

    def test_profile_has_resolutions(self, dbus_client: RatbagDBusClient):
        """Profile should expose resolution child paths."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        assert len(resolutions) == 2

    def test_profile_has_buttons(self, dbus_client: RatbagDBusClient):
        """Profile should expose button child paths."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        buttons = dbus_client.profile_buttons(profile)
        assert len(buttons) == 3

    def test_profile_has_leds(self, dbus_client: RatbagDBusClient):
        """Profile should expose LED child paths."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        assert len(leds) == 1


# ===========================================================================
# Resolution tests
# ===========================================================================


class TestResolution:
    """org.freedesktop.ratbag1.Resolution interface tests."""

    def test_resolution_index(self, dbus_client: RatbagDBusClient):
        """Resolution indices should be sequential."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        for i, rpath in enumerate(resolutions):
            assert dbus_client.resolution_index(rpath) == i

    def test_resolution_is_active(self, dbus_client: RatbagDBusClient):
        """First resolution should be active per spec."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        assert dbus_client.resolution_is_active(resolutions[0]) is True
        assert dbus_client.resolution_is_active(resolutions[1]) is False

    def test_resolution_is_default(self, dbus_client: RatbagDBusClient):
        """First resolution should be default per spec."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        assert dbus_client.resolution_is_default(resolutions[0]) is True
        assert dbus_client.resolution_is_default(resolutions[1]) is False

    def test_resolution_unified_dpi(self, dbus_client: RatbagDBusClient):
        """Unified DPI should return a single integer."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        val = dbus_client.resolution_value(resolutions[0])
        # Unified DPI → single uint32
        assert int(val) == 800

    def test_resolution_separate_dpi(self, dbus_client: RatbagDBusClient):
        """Separate X/Y DPI should return a two-element tuple."""
        path = _load_and_get_device(dbus_client, SEPARATE_DPI_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        val = dbus_client.resolution_value(resolutions[0])
        # Separate DPI → struct (u32, u32)
        assert int(val[0]) == 800
        assert int(val[1]) == 1600

    def test_set_resolution_unified(self, dbus_client: RatbagDBusClient):
        """Setting a unified DPI should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        dbus_client.set_resolution_value(resolutions[0], 1600)
        assert int(dbus_client.resolution_value(resolutions[0])) == 1600

    def test_set_resolution_separate(self, dbus_client: RatbagDBusClient):
        """Setting separate X/Y DPI should persist."""
        path = _load_and_get_device(dbus_client, SEPARATE_DPI_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        dbus_client.set_resolution_value(resolutions[0], (400, 3200))
        val = dbus_client.resolution_value(resolutions[0])
        assert int(val[0]) == 400
        assert int(val[1]) == 3200

    def test_resolution_set_active(self, dbus_client: RatbagDBusClient):
        """SetActive should switch active resolution."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        assert dbus_client.resolution_is_active(resolutions[0]) is True

        dbus_client.resolution_set_active(resolutions[1])
        assert dbus_client.resolution_is_active(resolutions[0]) is False
        assert dbus_client.resolution_is_active(resolutions[1]) is True

    def test_resolution_set_default(self, dbus_client: RatbagDBusClient):
        """SetDefault should switch default resolution."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        assert dbus_client.resolution_is_default(resolutions[0]) is True

        dbus_client.resolution_set_default(resolutions[1])
        assert dbus_client.resolution_is_default(resolutions[0]) is False
        assert dbus_client.resolution_is_default(resolutions[1]) is True

    def test_resolution_dpi_list(self, dbus_client: RatbagDBusClient):
        """Supported DPI list should contain values in range."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        dpi_list = dbus_client.resolution_dpi_list(resolutions[0])
        assert len(dpi_list) > 0
        assert min(dpi_list) >= 100
        assert max(dpi_list) <= 16000

    def test_resolution_is_disabled(self, dbus_client: RatbagDBusClient):
        """Resolution should not be disabled initially."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        assert dbus_client.resolution_is_disabled(resolutions[0]) is False

    def test_set_resolution_disabled(self, dbus_client: RatbagDBusClient):
        """Setting IsDisabled should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        dbus_client.set_resolution_is_disabled(resolutions[1], True)
        assert dbus_client.resolution_is_disabled(resolutions[1]) is True
