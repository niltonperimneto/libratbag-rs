"""
Tests for the org.freedesktop.ratbag1.Button and Led DBus interfaces.

Validates button action types, mapping read/write, LED mode, colors,
brightness, and effect duration on injected test devices.
"""

import time

import dbus
import pytest

from .conftest import SIMPLE_DEVICE_JSON, MULTI_PROFILE_DEVICE_JSON
from .ratbag_dbus import RatbagDBusClient

pytestmark = pytest.mark.requires_dev_hooks


# ---------------------------------------------------------------------------
# Constants matching the Rust enums
# ---------------------------------------------------------------------------

ACTION_NONE = 0
ACTION_BUTTON = 1
ACTION_SPECIAL = 2
ACTION_KEY = 3
ACTION_MACRO = 4

LED_OFF = 0
LED_SOLID = 1
LED_CYCLE = 2
LED_COLOR_WAVE = 4
LED_STARLIGHT = 5
LED_BREATHING = 3
LED_TRICOLOR = 6


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
# Button tests
# ===========================================================================


class TestButton:
    """org.freedesktop.ratbag1.Button interface tests."""

    def test_button_index(self, dbus_client: RatbagDBusClient):
        """Button indices should be sequential."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        buttons = dbus_client.profile_buttons(profile)
        for i, bpath in enumerate(buttons):
            assert dbus_client.button_index(bpath) == i

    def test_button_mapping_initial(self, dbus_client: RatbagDBusClient):
        """Initial button mapping should match the spec."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        buttons = dbus_client.profile_buttons(profile)
        mapping = dbus_client.button_mapping(buttons[0])
        action_type = int(mapping[0])
        assert action_type == ACTION_BUTTON
        assert int(mapping[1]) == 0x110

    def test_button_action_types(self, dbus_client: RatbagDBusClient):
        """ActionTypes should list all supported action types."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        buttons = dbus_client.profile_buttons(profile)
        types = dbus_client.button_action_types(buttons[0])
        # Test devices support all action types [0,1,2,3,4]
        assert ACTION_NONE in types
        assert ACTION_BUTTON in types
        assert ACTION_SPECIAL in types
        assert ACTION_KEY in types
        assert ACTION_MACRO in types

    def test_set_button_mapping_button(self, dbus_client: RatbagDBusClient):
        """Setting a button mapping to a different button should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        buttons = dbus_client.profile_buttons(profile)
        dbus_client.set_button_mapping(buttons[0], ACTION_BUTTON, 0x111)
        mapping = dbus_client.button_mapping(buttons[0])
        assert int(mapping[0]) == ACTION_BUTTON
        assert int(mapping[1]) == 0x111

    def test_set_button_mapping_key(self, dbus_client: RatbagDBusClient):
        """Setting a button to Key action type should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        buttons = dbus_client.profile_buttons(profile)
        # KEY_A = 30 in Linux input event codes
        dbus_client.set_button_mapping(buttons[0], ACTION_KEY, 30)
        mapping = dbus_client.button_mapping(buttons[0])
        assert int(mapping[0]) == ACTION_KEY
        assert int(mapping[1]) == 30

    def test_set_button_mapping_special(self, dbus_client: RatbagDBusClient):
        """Setting a button to Special action type should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        buttons = dbus_client.profile_buttons(profile)
        dbus_client.set_button_mapping(buttons[0], ACTION_SPECIAL, 42)
        mapping = dbus_client.button_mapping(buttons[0])
        assert int(mapping[0]) == ACTION_SPECIAL
        assert int(mapping[1]) == 42

    def test_set_button_mapping_none(self, dbus_client: RatbagDBusClient):
        """Setting a button to None action type should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        buttons = dbus_client.profile_buttons(profile)
        dbus_client.set_button_mapping(buttons[0], ACTION_NONE, 0)
        mapping = dbus_client.button_mapping(buttons[0])
        assert int(mapping[0]) == ACTION_NONE

    def test_set_button_mapping_none_normalizes_value(
        self, dbus_client: RatbagDBusClient
    ):
        """None action should always expose value 0 in Mapping."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        buttons = dbus_client.profile_buttons(profile)
        dbus_client.set_button_mapping(buttons[0], ACTION_NONE, 12345)
        mapping = dbus_client.button_mapping(buttons[0])
        assert int(mapping[0]) == ACTION_NONE
        assert int(mapping[1]) == 0

    def test_set_button_mapping_macro(self, dbus_client: RatbagDBusClient):
        """Setting a button to Macro action type should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        buttons = dbus_client.profile_buttons(profile)
        events = [(1, 30), (2, 30), (3, 50)]
        dbus_client.set_button_mapping(buttons[0], ACTION_MACRO, events)
        mapping = dbus_client.button_mapping(buttons[0])
        assert int(mapping[0]) == ACTION_MACRO
        assert [(int(a), int(b)) for a, b in mapping[1]] == events

    def test_set_button_mapping_unknown_type_rejected(
        self, dbus_client: RatbagDBusClient
    ):
        """Unsupported action type should be rejected and not mutate state."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        buttons = dbus_client.profile_buttons(profile)
        before = dbus_client.button_mapping(buttons[0])

        with pytest.raises(dbus.exceptions.DBusException):
            dbus_client.set_button_mapping(buttons[0], 999, 1)

        after = dbus_client.button_mapping(buttons[0])
        assert int(after[0]) == int(before[0])
        assert int(after[1]) == int(before[1])

    def test_button_none_in_multi_profile(self, dbus_client: RatbagDBusClient):
        """Third profile's button should have action type None."""
        path = _load_and_get_device(dbus_client, MULTI_PROFILE_DEVICE_JSON)
        profiles = dbus_client.device_profiles(path)
        buttons = dbus_client.profile_buttons(profiles[2])
        mapping = dbus_client.button_mapping(buttons[0])
        assert int(mapping[0]) == ACTION_NONE


# ===========================================================================
# LED tests
# ===========================================================================


class TestLed:
    """org.freedesktop.ratbag1.Led interface tests."""

    def test_led_index(self, dbus_client: RatbagDBusClient):
        """LED index should be 0 for the first LED."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        assert len(leds) == 1
        assert dbus_client.led_index(leds[0]) == 0

    def test_led_mode_initial(self, dbus_client: RatbagDBusClient):
        """LED mode should match the spec (Solid = 1)."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        assert dbus_client.led_mode(leds[0]) == LED_SOLID

    def test_led_modes_list(self, dbus_client: RatbagDBusClient):
        """Supported modes should include Off, Solid, Cycle, etc."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        modes = dbus_client.led_modes(leds[0])
        assert LED_OFF in modes
        assert LED_SOLID in modes
        assert LED_CYCLE in modes
        assert LED_BREATHING in modes

    def test_set_led_mode(self, dbus_client: RatbagDBusClient):
        """Changing LED mode should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        dbus_client.set_led_mode(leds[0], LED_BREATHING)
        assert dbus_client.led_mode(leds[0]) == LED_BREATHING

    def test_set_led_mode_invalid_rejected(self, dbus_client: RatbagDBusClient):
        """Setting a completely invalid mode number should raise an error."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        with pytest.raises(Exception):
            dbus_client.set_led_mode(leds[0], 9999)

    def test_set_led_mode_unsupported_rejected(self, dbus_client: RatbagDBusClient):
        """Setting a valid mode not in the device's modes list should fail."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        modes = dbus_client.led_modes(leds[0])
        # LED_TRICOLOR is a valid LedMode but not in test device's modes list
        assert LED_TRICOLOR not in modes
        with pytest.raises(Exception):
            dbus_client.set_led_mode(leds[0], LED_TRICOLOR)

    def test_led_color_initial(self, dbus_client: RatbagDBusClient):
        """LED color should match the spec (255, 0, 0)."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        r, g, b = dbus_client.led_color(leds[0])
        assert r == 255
        assert g == 0
        assert b == 0

    def test_set_led_color(self, dbus_client: RatbagDBusClient):
        """Setting LED color should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        dbus_client.set_led_color(leds[0], 0, 128, 255)
        r, g, b = dbus_client.led_color(leds[0])
        assert r == 0
        assert g == 128
        assert b == 255

    def test_led_secondary_color(self, dbus_client: RatbagDBusClient):
        """Secondary color should default to (0, 0, 0) for test devices."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        r, g, b = dbus_client.led_secondary_color(leds[0])
        assert (r, g, b) == (0, 0, 0)

    def test_set_led_secondary_color(self, dbus_client: RatbagDBusClient):
        """Setting secondary color should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        dbus_client.set_led_secondary_color(leds[0], 10, 20, 30)
        r, g, b = dbus_client.led_secondary_color(leds[0])
        assert (r, g, b) == (10, 20, 30)

    def test_led_tertiary_color(self, dbus_client: RatbagDBusClient):
        """Tertiary color should default to (0, 0, 0) for test devices."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        r, g, b = dbus_client.led_tertiary_color(leds[0])
        assert (r, g, b) == (0, 0, 0)

    def test_set_led_tertiary_color(self, dbus_client: RatbagDBusClient):
        """Setting tertiary color should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        dbus_client.set_led_tertiary_color(leds[0], 100, 200, 50)
        r, g, b = dbus_client.led_tertiary_color(leds[0])
        assert (r, g, b) == (100, 200, 50)

    def test_led_brightness_initial(self, dbus_client: RatbagDBusClient):
        """LED brightness should match the spec."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        assert dbus_client.led_brightness(leds[0]) == 200

    def test_set_led_brightness(self, dbus_client: RatbagDBusClient):
        """Setting LED brightness should persist (clamped to 0-255)."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        dbus_client.set_led_brightness(leds[0], 42)
        assert dbus_client.led_brightness(leds[0]) == 42

    def test_led_brightness_clamped(self, dbus_client: RatbagDBusClient):
        """Brightness above 255 should be clamped to 255."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        dbus_client.set_led_brightness(leds[0], 999)
        assert dbus_client.led_brightness(leds[0]) == 255

    def test_led_effect_duration_initial(self, dbus_client: RatbagDBusClient):
        """Effect duration should match the spec (1000 ms)."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        assert dbus_client.led_effect_duration(leds[0]) == 1000

    def test_set_led_effect_duration(self, dbus_client: RatbagDBusClient):
        """Setting effect duration should persist."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        dbus_client.set_led_effect_duration(leds[0], 5000)
        assert dbus_client.led_effect_duration(leds[0]) == 5000

    def test_led_effect_duration_clamped(self, dbus_client: RatbagDBusClient):
        """Effect duration above 10000 should be clamped to 10000."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        dbus_client.set_led_effect_duration(leds[0], 99999)
        assert dbus_client.led_effect_duration(leds[0]) == 10000

    def test_led_color_depth(self, dbus_client: RatbagDBusClient):
        """Color depth should be a non-negative integer."""
        path = _load_and_get_device(dbus_client, SIMPLE_DEVICE_JSON)
        profile = _first_profile(dbus_client, path)
        leds = dbus_client.profile_leds(profile)
        depth = dbus_client.led_color_depth(leds[0])
        assert depth >= 0

    def test_led_in_second_profile(self, dbus_client: RatbagDBusClient):
        """LEDs in the second profile should be independently accessible."""
        path = _load_and_get_device(dbus_client, MULTI_PROFILE_DEVICE_JSON)
        profiles = dbus_client.device_profiles(path)
        leds = dbus_client.profile_leds(profiles[1])
        assert len(leds) == 1
        r, g, b = dbus_client.led_color(leds[0])
        assert r == 0 and g == 255 and b == 0
