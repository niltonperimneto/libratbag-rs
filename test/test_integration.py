"""
Integration tests exercising cross-object mutations and commit workflows.

These tests verify that changes to child objects (resolutions, buttons, LEDs)
mark the parent profile dirty, that SetActive on profiles and resolutions
de-activates siblings, and that the full load→mutate→readback cycle works.
"""

import json
import time

import pytest

from .conftest import SIMPLE_DEVICE_JSON, MULTI_PROFILE_DEVICE_JSON
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
    assert profiles
    return profiles[0]


# ===========================================================================
# Cross-object mutation tests
# ===========================================================================


class TestCrossObjectMutations:
    """Tests that verify property changes propagate correctly across objects."""

    def test_resolution_change_dirties_profile(
        self, dbus_client: RatbagDBusClient
    ):
        """Changing a resolution's DPI should mark the profile as dirty."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        dbus_client.set_resolution_value(resolutions[0], 3200)
        assert dbus_client.profile_is_dirty(profile) is True

    def test_button_change_dirties_profile(
        self, dbus_client: RatbagDBusClient
    ):
        """Changing a button's mapping should mark the profile as dirty."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        buttons = dbus_client.profile_buttons(profile)
        dbus_client.set_button_mapping(buttons[0], 2, 99)  # Special(99)
        assert dbus_client.profile_is_dirty(profile) is True

    def test_led_change_dirties_profile(
        self, dbus_client: RatbagDBusClient
    ):
        """Changing an LED's color should mark the profile as dirty."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        dbus_client.set_led_color(leds[0], 42, 42, 42)
        assert dbus_client.profile_is_dirty(profile) is True

    def test_set_active_deactivates_siblings(
        self, dbus_client: RatbagDBusClient
    ):
        """SetActive on profile N should deactivate all other profiles."""
        path = _load_and_get_device(dbus_client, MULTI_PROFILE_DEVICE_JSON)
        profiles = dbus_client.device_profiles(path)

        # Initially profile 0 is active
        assert dbus_client.profile_is_active(profiles[0]) is True

        # Activate profile 1
        dbus_client.profile_set_active(profiles[1])
        for i, p in enumerate(profiles):
            if i == 1:
                assert dbus_client.profile_is_active(p) is True
            else:
                assert dbus_client.profile_is_active(p) is False

    def test_resolution_set_active_deactivates_siblings(
        self, dbus_client: RatbagDBusClient
    ):
        """SetActive on resolution N should deactivate sibling resolutions."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)

        assert dbus_client.resolution_is_active(resolutions[0]) is True
        dbus_client.resolution_set_active(resolutions[1])
        assert dbus_client.resolution_is_active(resolutions[0]) is False
        assert dbus_client.resolution_is_active(resolutions[1]) is True

    def test_resolution_set_default_clears_siblings(
        self, dbus_client: RatbagDBusClient
    ):
        """SetDefault on resolution N should clear default from siblings."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)

        assert dbus_client.resolution_is_default(resolutions[0]) is True
        dbus_client.resolution_set_default(resolutions[1])
        assert dbus_client.resolution_is_default(resolutions[0]) is False
        assert dbus_client.resolution_is_default(resolutions[1]) is True


# ===========================================================================
# Full round-trip integration tests
# ===========================================================================


class TestRoundTrip:
    """End-to-end tests: load device, mutate, read back, and commit."""

    def test_full_mutation_round_trip(self, dbus_client: RatbagDBusClient):
        """Load a device, change several properties, verify they persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)

        # Mutate profile
        dbus_client.set_profile_name(profile, "FPS Mode")
        dbus_client.set_profile_report_rate(profile, 500)

        # Mutate resolution
        resolutions = dbus_client.profile_resolutions(profile)
        dbus_client.set_resolution_value(resolutions[0], 3200)

        # Mutate button
        buttons = dbus_client.profile_buttons(profile)
        dbus_client.set_button_mapping(buttons[1], 3, 30)  # Key(KEY_A)

        # Mutate LED
        leds = dbus_client.profile_leds(profile)
        dbus_client.set_led_mode(leds[0], 3)  # Breathing
        dbus_client.set_led_color(leds[0], 0, 255, 128)
        dbus_client.set_led_brightness(leds[0], 180)

        # Read everything back
        assert dbus_client.profile_name(profile) == "FPS Mode"
        assert dbus_client.profile_report_rate(profile) == 500
        assert int(dbus_client.resolution_value(resolutions[0])) == 3200

        mapping = dbus_client.button_mapping(buttons[1])
        assert int(mapping[0]) == 3  # Key
        assert int(mapping[1]) == 30

        assert dbus_client.led_mode(leds[0]) == 3
        r, g, b = dbus_client.led_color(leds[0])
        assert (r, g, b) == (0, 255, 128)
        assert dbus_client.led_brightness(leds[0]) == 180

        # Profile should be dirty
        assert dbus_client.profile_is_dirty(profile) is True

    def test_commit_returns_status(self, dbus_client: RatbagDBusClient):
        """Commit should return an integer status code."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        result = dbus_client.device_commit(path)
        assert isinstance(result, int)

    def test_reload_device_resets_state(self, dbus_client: RatbagDBusClient):
        """Loading a new test device should provide fresh default state."""
        path1 = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile1 = _first_profile(dbus_client, path1)
        dbus_client.set_profile_name(profile1, "Modified")

        # Load a fresh device
        path2 = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile2 = _first_profile(dbus_client, path2)
        # New device should have default (empty) name
        assert dbus_client.profile_name(profile2) == ""


# ===========================================================================
# Edge case / robustness tests
# ===========================================================================


class TestEdgeCases:
    """Edge cases and boundary conditions."""

    def test_empty_json_produces_valid_device(
        self, dbus_client: RatbagDBusClient
    ):
        """An empty JSON string should produce a valid minimal device."""
        path = _load_and_get_device(dbus_client, "")
        profiles = dbus_client.device_profiles(path)
        assert len(profiles) >= 1

    def test_empty_object_json(self, dbus_client: RatbagDBusClient):
        """An empty JSON object {} should produce a valid device."""
        path = _load_and_get_device(dbus_client, "{}")
        profiles = dbus_client.device_profiles(path)
        assert len(profiles) >= 1

    def test_many_profiles(self, dbus_client: RatbagDBusClient):
        """A device with many profiles should enumerate correctly."""
        spec = {
            "profiles": [
                {
                    "is_active": i == 0,
                    "rate": 1000,
                    "resolutions": [
                        {"xres": 800, "yres": 800, "is_active": True, "is_default": True}
                    ],
                    "buttons": [{"action_type": "button", "button": 0x110}],
                }
                for i in range(5)
            ]
        }
        path = _load_and_get_device(dbus_client, json.dumps(spec))
        profiles = dbus_client.device_profiles(path)
        assert len(profiles) == 5

    def test_many_resolutions(self, dbus_client: RatbagDBusClient):
        """A profile with many resolutions should enumerate correctly."""
        spec = {
            "profiles": [
                {
                    "is_active": True,
                    "resolutions": [
                        {
                            "xres": dpi,
                            "yres": dpi,
                            "is_active": dpi == 400,
                            "is_default": dpi == 400,
                        }
                        for dpi in [400, 800, 1600, 3200]
                    ],
                    "buttons": [{"action_type": "button", "button": 0x110}],
                }
            ]
        }
        path = _load_and_get_device(dbus_client, json.dumps(spec))
        profile = _first_profile(dbus_client, path)
        resolutions = dbus_client.profile_resolutions(profile)
        assert len(resolutions) == 4

    def test_device_with_no_leds(self, dbus_client: RatbagDBusClient):
        """A profile with no LEDs should return an empty Leds list."""
        spec = {
            "profiles": [
                {
                    "is_active": True,
                    "resolutions": [
                        {"xres": 800, "yres": 800, "is_active": True, "is_default": True}
                    ],
                    "buttons": [{"action_type": "button", "button": 0x110}],
                    "leds": [],
                }
            ]
        }
        path = _load_and_get_device(dbus_client, json.dumps(spec))
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        assert len(leds) == 0

    def test_multiple_leds(self, dbus_client: RatbagDBusClient):
        """Multiple LEDs per profile should work."""
        spec = {
            "profiles": [
                {
                    "is_active": True,
                    "resolutions": [
                        {"xres": 800, "yres": 800, "is_active": True, "is_default": True}
                    ],
                    "buttons": [{"action_type": "button", "button": 0x110}],
                    "leds": [
                        {"mode": 1, "color": [255, 0, 0], "brightness": 100},
                        {"mode": 0, "color": [0, 0, 255], "brightness": 50},
                        {"mode": 3, "color": [0, 255, 0], "brightness": 200},
                    ],
                }
            ]
        }
        path = _load_and_get_device(dbus_client, json.dumps(spec))
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        assert len(leds) == 3

        # Verify each LED has correct initial values
        assert dbus_client.led_mode(leds[0]) == 1  # Solid
        assert dbus_client.led_mode(leds[1]) == 0  # Off
        assert dbus_client.led_mode(leds[2]) == 3  # Cycle

        assert dbus_client.led_color(leds[0]) == (255, 0, 0)
        assert dbus_client.led_color(leds[1]) == (0, 0, 255)
        assert dbus_client.led_color(leds[2]) == (0, 255, 0)
