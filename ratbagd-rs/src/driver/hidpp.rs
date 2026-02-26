/* Shared HID++ protocol definitions for both 1.0 and 2.0. */
/*  */
/* HID++ uses two report formats: */
/* - Short (Report ID 0x10): 7 bytes total */
/* - Long  (Report ID 0x11): 20 bytes total */

/* HID++ report IDs */
pub const REPORT_ID_SHORT: u8 = 0x10;
pub const REPORT_ID_LONG: u8 = 0x11;

/* HID++ error sub-IDs */
pub const HIDPP10_ERROR: u8 = 0x8F;
pub const HIDPP20_ERROR: u8 = 0xFF;

/* Well-known device indices */
pub const DEVICE_IDX_WIRED: u8 = 0x00;

/* HID++ 2.0 feature pages */
pub const PAGE_DEVICE_NAME: u16 = 0x0005;
pub const PAGE_SPECIAL_KEYS_BUTTONS: u16 = 0x1B04;
pub const PAGE_ADJUSTABLE_DPI: u16 = 0x2201;
pub const PAGE_ADJUSTABLE_REPORT_RATE: u16 = 0x8060;
pub const PAGE_COLOR_LED_EFFECTS: u16 = 0x8070;
pub const PAGE_RGB_EFFECTS: u16 = 0x8071;
pub const PAGE_ONBOARD_PROFILES: u16 = 0x8100;

/* Root feature index — always fixed at 0x00 */
pub const ROOT_FEATURE_INDEX: u8 = 0x00;

/* Root feature function IDs */
pub const ROOT_FN_GET_FEATURE: u8 = 0x00;
pub const ROOT_FN_GET_PROTOCOL_VERSION: u8 = 0x01;

/* -------------------------------------------------------------------------- */
/* LED protocol constants (from C library hidpp20.h)                          */
/* -------------------------------------------------------------------------- */

/* HID++ 2.0 LED hardware mode bytes (hidpp20_led_mode / hidpp20_color_led_zone_effect) */
pub const LED_HW_MODE_OFF: u8 = 0x00;
pub const LED_HW_MODE_FIXED: u8 = 0x01;
pub const LED_HW_MODE_CYCLE: u8 = 0x03;
pub const LED_HW_MODE_COLOR_WAVE: u8 = 0x04;
pub const LED_HW_MODE_STARLIGHT: u8 = 0x05;
pub const LED_HW_MODE_BREATHING: u8 = 0x0A;

/* 0x8070 Color LED Effects command IDs */
#[allow(dead_code)]
pub const CMD_COLOR_LED_EFFECTS_GET_ZONE_EFFECT: u8 = 0x01;
#[allow(dead_code)]
pub const CMD_COLOR_LED_EFFECTS_SET_ZONE_EFFECT: u8 = 0x02;

/* 0x8071 RGB Effects command IDs */
#[allow(dead_code)]
pub const CMD_RGB_EFFECTS_GET_INFO: u8 = 0x00;
#[allow(dead_code)]
pub const CMD_RGB_EFFECTS_SET_CLUSTER_EFFECT: u8 = 0x01;
#[allow(dead_code)]
pub const CMD_RGB_EFFECTS_SET_MULTI_LED_PATTERN: u8 = 0x02;

/* Size of the internal LED payload as defined in C struct hidpp20_internal_led. */
pub const LED_PAYLOAD_SIZE: usize = 11;

/* Build the 11-byte LED payload matching the C `struct hidpp20_internal_led`. */
/*  */
/* The byte layout for each mode is:                                          */
/* Off:       [0x00, 0..10 zero]                                              */
/* Solid:     [0x01, R, G, B, 0x00, 0..6 zero]                               */
/* Cycle:     [0x03, 0..5 zero, period_hi, period_lo, brightness, 0..2 zero]  */
/* ColorWave: [0x04, 0..5 zero, period_hi, period_lo, brightness, 0..2 zero]  */
/* Starlight: [0x05, sky_R, sky_G, sky_B, star_R, star_G, star_B, 0..4 zero]  */
/* Breathing: [0x0A, R, G, B, period_hi, period_lo, waveform, brightness, 0..3]*/
pub fn build_led_payload(led: &crate::device::LedInfo) -> [u8; LED_PAYLOAD_SIZE] {
    use crate::device::LedMode;

    let mut payload = [0u8; LED_PAYLOAD_SIZE];
    let rgb = led.color.to_rgb();
    let period = (led.effect_duration as u16).to_be_bytes();
    let brightness = (led.brightness.min(255) * 100 / 255) as u8;

    match led.mode {
        LedMode::Off => {
            payload[0] = LED_HW_MODE_OFF;
        }
        LedMode::Solid => {
            payload[0] = LED_HW_MODE_FIXED;
            payload[1] = rgb.r;
            payload[2] = rgb.g;
            payload[3] = rgb.b;
        }
        LedMode::Cycle => {
            payload[0] = LED_HW_MODE_CYCLE;
            payload[6] = period[0];
            payload[7] = period[1];
            payload[8] = brightness;
        }
        LedMode::ColorWave => {
            payload[0] = LED_HW_MODE_COLOR_WAVE;
            payload[6] = period[0];
            payload[7] = period[1];
            payload[8] = brightness;
        }
        LedMode::Starlight => {
            let star = led.secondary_color.to_rgb();
            payload[0] = LED_HW_MODE_STARLIGHT;
            payload[1] = rgb.r;
            payload[2] = rgb.g;
            payload[3] = rgb.b;
            payload[4] = star.r;
            payload[5] = star.g;
            payload[6] = star.b;
        }
        LedMode::Breathing => {
            payload[0] = LED_HW_MODE_BREATHING;
            payload[1] = rgb.r;
            payload[2] = rgb.g;
            payload[3] = rgb.b;
            payload[4] = period[0];
            payload[5] = period[1];
            /* waveform defaults to 0x00 (default sine) */
            payload[7] = brightness;
        }
        LedMode::TriColor => {
            /* TriColor uses the full 9-byte RGB for 3 zones: left, center, right. */
            /* Primary = left, secondary = center, tertiary = right. */
            let center = led.secondary_color.to_rgb();
            let right = led.tertiary_color.to_rgb();
            payload[0] = LED_HW_MODE_FIXED;
            payload[1] = rgb.r;
            payload[2] = rgb.g;
            payload[3] = rgb.b;
            payload[4] = center.r;
            payload[5] = center.g;
            payload[6] = center.b;
            payload[7] = right.r;
            payload[8] = right.g;
            payload[9] = right.b;
        }
    }

    payload
}

/* A parsed HID++ report. */
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HidppReport {
    /* Short report (7 bytes, report ID 0x10). */
    Short {
        device_index: u8,
        sub_id: u8,
        params: [u8; 3],
    },
    /* Long report (20 bytes, report ID 0x11). */
    Long {
        device_index: u8,
        sub_id: u8,
        address: u8,
        params: [u8; 16],
    },
}

impl HidppReport {
    /* Try to parse a raw byte buffer into a structured report. */
    /* Returns `None` if the buffer is too short or has an */
    /* unrecognised report ID. */
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 7 {
            return None;
        }

        match buf[0] {
            REPORT_ID_SHORT => Some(Self::Short {
                device_index: buf[1],
                sub_id: buf[2],
                params: [buf[3], buf[4], buf[5]],
            }),
            REPORT_ID_LONG if buf.len() >= 20 => {
                let mut params = [0u8; 16];
                params.copy_from_slice(&buf[4..20]);
                Some(Self::Long {
                    device_index: buf[1],
                    sub_id: buf[2],
                    address: buf[3],
                    params,
                })
            }
            _ => None,
        }
    }

    /* Check if this report is an error response (0x8F for short, 0xFF for long). */
    pub fn is_error(&self) -> bool {
        match self {
            Self::Short { sub_id, .. } => *sub_id == HIDPP10_ERROR,
            Self::Long { sub_id, .. } => *sub_id == HIDPP20_ERROR,
        }
    }

    /* For a HID++ 2.0 long report, returns true if it is a response */
    /* matching the given device index and feature index. */
    pub fn matches_hidpp20(&self, expected_dev: u8, expected_feature: u8) -> bool {
        matches!(
            self,
            Self::Long { device_index, sub_id, .. }
                if *device_index == expected_dev && *sub_id == expected_feature
        )
    }
}

/* Build a 7-byte HID++ short report. */
pub fn build_short_report(device_index: u8, sub_id: u8, params: [u8; 3]) -> [u8; 7] {
    [
        REPORT_ID_SHORT,
        device_index,
        sub_id,
        params[0],
        params[1],
        params[2],
        0x00,
    ]
}

/* Build a HID++ 2.0 feature request. */
/*  */
/* Layout: `[0x11, device_idx, feature_idx, (function << 4 | sw_id), params...]` */
pub fn build_hidpp20_request(
    device_index: u8,
    feature_index: u8,
    function: u8,
    sw_id: u8,
    params: &[u8],
) -> [u8; 20] {
    let mut buf = [0u8; 20];
    buf[0] = REPORT_ID_LONG;
    buf[1] = device_index;
    buf[2] = feature_index;
    buf[3] = (function << 4) | (sw_id & 0x0F);
    let copy_len = params.len().min(16);
    buf[4..4 + copy_len].copy_from_slice(&params[..copy_len]);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_short_report() {
        let buf = [0x10, 0x00, 0x01, 0xAA, 0xBB, 0xCC, 0x00];
        let report = HidppReport::parse(&buf).expect("valid short report");
        assert_eq!(
            report,
            HidppReport::Short {
                device_index: 0x00,
                sub_id: 0x01,
                params: [0xAA, 0xBB, 0xCC],
            }
        );
    }

    #[test]
    fn parse_long_report() {
        let mut buf = [0u8; 20];
        buf[0] = REPORT_ID_LONG;
        buf[1] = 0x02;
        buf[2] = 0x03;
        buf[3] = 0xFF;
        let report = HidppReport::parse(&buf).expect("valid long report");
        match report {
            HidppReport::Long { device_index, sub_id, address, params } => {
                assert_eq!(device_index, 0x02);
                assert_eq!(sub_id, 0x03);
                assert_eq!(address, 0xFF);
                assert_eq!(params[0], 0x00);
            }
            _ => panic!("Expected Long report"),
        }
    }

    #[test]
    fn parse_invalid_report_id() {
        let buf = [0x99, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(HidppReport::parse(&buf).is_none());
    }

    #[test]
    fn parse_buffer_too_short() {
        assert!(HidppReport::parse(&[0x10, 0x00]).is_none());
        assert!(HidppReport::parse(&[]).is_none());
    }

    #[test]
    fn build_short_roundtrip() {
        let report = build_short_report(0x00, 0x01, [0xAA, 0xBB, 0xCC]);
        let parsed = HidppReport::parse(&report).expect("roundtrip");
        match parsed {
            HidppReport::Short { device_index, sub_id, .. } => {
                assert_eq!(device_index, 0x00);
                assert_eq!(sub_id, 0x01);
            }
            _ => panic!("Expected Short report"),
        }
    }

    #[test]
    fn build_hidpp20_request_encoding() {
        let req = build_hidpp20_request(0x00, 0x01, 0x02, 0x0A, &[0x11, 0x22]);
        assert_eq!(req[0], REPORT_ID_LONG);
        assert_eq!(req[1], 0x00);
        assert_eq!(req[2], 0x01);
        /* function=0x02, sw_id=0x0A → (0x02 << 4) | 0x0A = 0x2A */
        assert_eq!(req[3], 0x2A);
        assert_eq!(req[4], 0x11);
        assert_eq!(req[5], 0x22);
    }

    #[test]
    fn error_detection() {
        let err_short = HidppReport::Short {
            device_index: 0,
            sub_id: HIDPP10_ERROR,
            params: [0; 3],
        };
        assert!(err_short.is_error());

        let ok_short = HidppReport::Short {
            device_index: 0,
            sub_id: 0x01,
            params: [0; 3],
        };
        assert!(!ok_short.is_error());
    }

    #[test]
    fn matches_hidpp20_helper() {
        let report = HidppReport::Long {
            device_index: 0x00,
            sub_id: 0x05,
            address: 0x10,
            params: [0; 16],
        };
        assert!(report.matches_hidpp20(0x00, 0x05));
        assert!(!report.matches_hidpp20(0x00, 0x06));
        assert!(!report.matches_hidpp20(0x01, 0x05));
    }

    /* ------------------------------------------------------------------ */
    /* LED payload serialization tests                                    */
    /* ------------------------------------------------------------------ */

    use crate::device::{Color, LedInfo, LedMode};

    fn make_led(mode: LedMode) -> LedInfo {
        LedInfo {
            index: 0,
            mode,
            modes: vec![LedMode::Off],
            color: Color::default(),
            secondary_color: Color::default(),
            tertiary_color: Color::default(),
            color_depth: 1,
            effect_duration: 0,
            brightness: 255,
        }
    }

    #[test]
    fn led_payload_off() {
        let led = make_led(LedMode::Off);
        let p = build_led_payload(&led);
        assert_eq!(p, [0x00; LED_PAYLOAD_SIZE]);
    }

    #[test]
    fn led_payload_solid() {
        let mut led = make_led(LedMode::Solid);
        led.color = Color { red: 255, green: 128, blue: 0 };
        let p = build_led_payload(&led);
        assert_eq!(p[0], LED_HW_MODE_FIXED);
        assert_eq!(p[1], 255);
        assert_eq!(p[2], 128);
        assert_eq!(p[3], 0);
    }

    #[test]
    fn led_payload_cycle() {
        let mut led = make_led(LedMode::Cycle);
        led.effect_duration = 5000;
        led.brightness = 255;
        let p = build_led_payload(&led);
        assert_eq!(p[0], LED_HW_MODE_CYCLE);
        /* period 5000 = 0x1388 big-endian */
        assert_eq!(p[6], 0x13);
        assert_eq!(p[7], 0x88);
        /* brightness 255 → 100% */
        assert_eq!(p[8], 100);
    }

    #[test]
    fn led_payload_color_wave() {
        let mut led = make_led(LedMode::ColorWave);
        led.effect_duration = 3000;
        led.brightness = 127;
        let p = build_led_payload(&led);
        assert_eq!(p[0], LED_HW_MODE_COLOR_WAVE);
        assert_eq!(p[6], 0x0B);
        assert_eq!(p[7], 0xB8);
        /* brightness 127 → 127*100/255 = 49 */
        assert_eq!(p[8], 49);
    }

    #[test]
    fn led_payload_starlight() {
        let mut led = make_led(LedMode::Starlight);
        led.color = Color { red: 10, green: 20, blue: 30 };
        led.secondary_color = Color { red: 40, green: 50, blue: 60 };
        let p = build_led_payload(&led);
        assert_eq!(p[0], LED_HW_MODE_STARLIGHT);
        /* sky color */
        assert_eq!(p[1], 10);
        assert_eq!(p[2], 20);
        assert_eq!(p[3], 30);
        /* star color */
        assert_eq!(p[4], 40);
        assert_eq!(p[5], 50);
        assert_eq!(p[6], 60);
    }

    #[test]
    fn led_payload_breathing() {
        let mut led = make_led(LedMode::Breathing);
        led.color = Color { red: 0, green: 255, blue: 0 };
        led.effect_duration = 2000;
        led.brightness = 200;
        let p = build_led_payload(&led);
        assert_eq!(p[0], LED_HW_MODE_BREATHING);
        assert_eq!(p[1], 0);
        assert_eq!(p[2], 255);
        assert_eq!(p[3], 0);
        /* period 2000 = 0x07D0 */
        assert_eq!(p[4], 0x07);
        assert_eq!(p[5], 0xD0);
        /* waveform = 0x00 (default) at [6] */
        assert_eq!(p[6], 0x00);
        /* brightness 200 → 200*100/255 = 78 */
        assert_eq!(p[7], 78);
    }

    #[test]
    fn led_payload_tricolor() {
        let mut led = make_led(LedMode::TriColor);
        led.color = Color { red: 255, green: 0, blue: 0 };
        led.secondary_color = Color { red: 0, green: 255, blue: 0 };
        led.tertiary_color = Color { red: 0, green: 0, blue: 255 };
        let p = build_led_payload(&led);
        /* TriColor serializes as FIXED mode byte */
        assert_eq!(p[0], LED_HW_MODE_FIXED);
        /* left (primary) */
        assert_eq!(p[1], 255);
        assert_eq!(p[2], 0);
        assert_eq!(p[3], 0);
        /* center (secondary) */
        assert_eq!(p[4], 0);
        assert_eq!(p[5], 255);
        assert_eq!(p[6], 0);
        /* right (tertiary) */
        assert_eq!(p[7], 0);
        assert_eq!(p[8], 0);
        assert_eq!(p[9], 255);
    }
}
