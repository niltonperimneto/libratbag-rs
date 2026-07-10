/* SteelSeries mouse driver.
 *
 * SteelSeries spread their configuration protocol across four incompatible
 * "versions" (selected by the device database's DeviceVersion field). They
 * share a command vocabulary (set DPI / report rate / LED / buttons / save)
 * but differ in opcodes, report sizes, byte offsets, and whether a command
 * travels as an interrupt OUTPUT report or a HID FEATURE report.
 *
 * Wire framing (see the C union `steelseries_message`):
 *  - OUTPUT reports: byte 0 is the report id (always 0x00), byte 1 is the
 *    opcode, and the C `parameters[]` array begins at the opcode. So the C
 *    `parameters[i]` lives at our `buf[1 + i]`.
 *  - FEATURE reports (protocol V3): the opcode *is* the HID feature report
 *    number at byte 0, and `parameters[i]` lives at `buf[i]`.
 *
 * The `Report` builder below addresses slots by their C `parameters[]` index
 * regardless of framing, so the byte math here lines up 1:1 with libratbag and
 * the per-call "+1 offset" bookkeeping disappears.
 *
 * Every command must be spaced out by a short settle delay; `dispatch()` folds
 * that in so individual writers never sleep by hand. There is no host-side
 * cache: `load_profiles` seeds defaults and then overlays whatever the hardware
 * reports, and `commit` writes the active profile straight to the device. */

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use tracing::{debug, warn};

use crate::engine::device::{
    ActionType, ButtonInfo, Color, DeviceInfo, Dpi, LedInfo, LedMode, ProfileInfo, ResolutionInfo,
    special_action,
};
use crate::hal::{DeviceDriver, DeviceIo};

/* ---------------------------------------------------------------------- */
/* Constants                                                              */
/* ---------------------------------------------------------------------- */
const STEELSERIES_NUM_PROFILES: u8 = 1;
const STEELSERIES_NUM_DPI: u8 = 2;

const STEELSERIES_REPORT_SIZE_SHORT: usize = 32;
const STEELSERIES_REPORT_SIZE: usize = 64;
const STEELSERIES_REPORT_LONG_SIZE: usize = 262;

/* Opcodes - V1 Short */
const STEELSERIES_ID_DPI_SHORT: u8 = 0x03;
const STEELSERIES_ID_REPORT_RATE_SHORT: u8 = 0x04;
const STEELSERIES_ID_LED_INTENSITY_SHORT: u8 = 0x05;
const STEELSERIES_ID_LED_EFFECT_SHORT: u8 = 0x07;
const STEELSERIES_ID_LED_COLOR_SHORT: u8 = 0x08;
const STEELSERIES_ID_LED_COLOR_SHORT_RIVAL100: u8 = 0x05;
const STEELSERIES_ID_SAVE_SHORT: u8 = 0x09;
const STEELSERIES_ID_FIRMWARE_PROTOCOL1: u8 = 0x10;

/* Opcodes - V2 */
const STEELSERIES_ID_BUTTONS: u8 = 0x31;
const STEELSERIES_ID_DPI: u8 = 0x53;
const STEELSERIES_ID_REPORT_RATE: u8 = 0x54;
const STEELSERIES_ID_LED: u8 = 0x5b;
const STEELSERIES_ID_SAVE: u8 = 0x59;
const STEELSERIES_ID_FIRMWARE_PROTOCOL2: u8 = 0x90;
const STEELSERIES_ID_SETTINGS: u8 = 0x92;

/* Opcodes - V3 */
const STEELSERIES_ID_DPI_PROTOCOL3: u8 = 0x03;
const STEELSERIES_ID_REPORT_RATE_PROTOCOL3: u8 = 0x04;
const STEELSERIES_ID_LED_PROTOCOL3: u8 = 0x05;
const STEELSERIES_ID_SAVE_PROTOCOL3: u8 = 0x09;
const STEELSERIES_ID_FIRMWARE_PROTOCOL3: u8 = 0x10;
const STEELSERIES_ID_SETTINGS_PROTOCOL3: u8 = 0x16;

/* Opcodes - V4 */
const STEELSERIES_ID_DPI_PROTOCOL4: u8 = 0x15;
const STEELSERIES_ID_REPORT_RATE_PROTOCOL4: u8 = 0x17;

/* Buttons */
const STEELSERIES_BUTTON_OFF: u8 = 0x00;
const STEELSERIES_BUTTON_RES_CYCLE: u8 = 0x30;
const STEELSERIES_BUTTON_WHEEL_UP: u8 = 0x31;
const STEELSERIES_BUTTON_WHEEL_DOWN: u8 = 0x32;
const STEELSERIES_BUTTON_KEY: u8 = 0x10;
const STEELSERIES_BUTTON_KBD: u8 = 0x51;
const STEELSERIES_BUTTON_CONSUMER: u8 = 0x61;

/* Button payload stride per button in the report (bytes) */
const STEELSERIES_BUTTON_SIZE_SENSEIRAW: usize = 3;
const STEELSERIES_BUTTON_SIZE_STANDARD: usize = 5;

/* DPI scaling: hardware stores (dpi / step) - 1; marker byte used by V2/V3 */
const STEELSERIES_DPI_MAGIC_MARKER: u8 = 0x42;

/* Inter-command settle delays. Every SteelSeries write needs a brief pause so
 * the firmware can absorb the previous command; saves to NVRAM want longer. */
const SETTLE: Duration = Duration::from_millis(10);
const SETTLE_SAVE: Duration = Duration::from_millis(20);

/* ---------------------------------------------------------------------- */
/* Report builder                                                         */
/* ---------------------------------------------------------------------- */

/* A SteelSeries command report under construction.
 *
 * Slots are addressed by their C `parameters[]` index (`parameters[0]` is the
 * opcode). The builder knows whether the report is OUTPUT- or FEATURE-framed
 * and places bytes accordingly, so callers never juggle the report-id offset. */
struct Report {
    buf: [u8; STEELSERIES_REPORT_LONG_SIZE],
    len: usize,
    /* Index of `parameters[0]` (the opcode): 1 for output reports (after the
     * report id), 0 for feature reports (the opcode is the report number). */
    base: usize,
    feature: bool,
    opcode: u8,
}

impl Report {
    /* OUTPUT report: byte 0 = report id (0x00), byte 1 = opcode. */
    fn output(opcode: u8, len: usize) -> Self {
        let mut buf = [0u8; STEELSERIES_REPORT_LONG_SIZE];
        buf[1] = opcode;
        Self { buf, len, base: 1, feature: false, opcode }
    }

    /* FEATURE report (V3): byte 0 = opcode = HID feature report number. */
    fn feature(opcode: u8, len: usize) -> Self {
        let mut buf = [0u8; STEELSERIES_REPORT_LONG_SIZE];
        buf[0] = opcode;
        Self { buf, len, base: 0, feature: true, opcode }
    }

    /* Write the C `parameters[i]` slot. */
    fn param(&mut self, i: usize, value: u8) -> &mut Self {
        let off = self.base + i;
        if off < self.len {
            self.buf[off] = value;
        }
        self
    }

    /* Write a little-endian u16 into `parameters[i..i+2]`. */
    fn param_u16_le(&mut self, i: usize, value: u16) -> &mut Self {
        let off = self.base + i;
        if off + 1 < self.len {
            self.buf[off..off + 2].copy_from_slice(&value.to_le_bytes());
        }
        self
    }

    /* Mutable view of the active payload, for the few writers that pack bytes
     * at computed offsets (button table, LED cycle points). */
    fn body_mut(&mut self) -> &mut [u8] {
        &mut self.buf[..self.len]
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/* ---------------------------------------------------------------------- */
/* Protocol version                                                       */
/* ---------------------------------------------------------------------- */

/* Wire protocol generation, selected by the device database's DeviceVersion
 * field.  The four generations share a command vocabulary but differ in
 * opcodes, report sizes, byte offsets, and framing (see the module header). */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtocolVersion {
    V1,
    V2,
    V3,
    V4,
}

impl TryFrom<u32> for ProtocolVersion {
    type Error = anyhow::Error; /* becomes SteelSeriesError in a later phase */

    fn try_from(value: u32) -> Result<Self> {
        match value {
            1 => Ok(Self::V1),
            2 => Ok(Self::V2),
            3 => Ok(Self::V3),
            4 => Ok(Self::V4),
            other => Err(anyhow!(
                "SteelSeries: unsupported DeviceVersion {other} (expected 1-4)"
            )),
        }
    }
}

/* ---------------------------------------------------------------------- */
/* Device quirks                                                          */
/* ---------------------------------------------------------------------- */

/* Per-device behavioral deviations, parsed once from the device database
 * entry's Quirks= list when load_profiles runs.  Command writers read these
 * booleans instead of re-scanning the quirk strings on every report. */
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DeviceQuirks {
    /* Sensei Raw: monochrome LED (intensity, not RGB), 3-byte button
     * records, no macro support. */
    is_senseiraw: bool,
    /* Rival 100: different V1 LED color opcode with a fixed led id of 0. */
    is_rival100: bool,
}

impl DeviceQuirks {
    fn from_config(config: &crate::engine::device_database::DriverConfig) -> Self {
        let has = |name: &str| config.quirks.iter().any(|q| q == name);
        Self {
            is_senseiraw: has("STEELSERIES_QUIRK_SENSEIRAW"),
            is_rival100: has("STEELSERIES_QUIRK_RIVAL100"),
        }
    }
}

/* ---------------------------------------------------------------------- */
/* Driver Instance                                                        */
/* ---------------------------------------------------------------------- */

pub struct SteelseriesDriver {
    /* None until load_profiles has parsed the device database entry. */
    version: Option<ProtocolVersion>,
    /* Parsed alongside `version`; only read from paths that already run
     * behind the version() initialization guard. */
    quirks: DeviceQuirks,
}

impl SteelseriesDriver {
    pub fn new() -> Self {
        Self { version: None, quirks: DeviceQuirks::default() }
    }

    /* The parsed protocol version.  Every command path runs after
     * load_profiles (the actor aborts device registration if it fails), so
     * this error is an initialization-order guard, not a runtime state. */
    fn version(&self) -> Result<ProtocolVersion> {
        self.version.ok_or_else(|| {
            anyhow!("SteelSeries: driver used before load_profiles initialized the protocol version")
        })
    }
}

/* Resolve the DPI step from the driver config.  Most SteelSeries devices
 * store the DPI index as (dpi / step - 1) where step comes from the
 * device database DpiRange.  Fallback to 100 if no range is configured. */
fn dpi_step(info: &DeviceInfo) -> u32 {
    info.driver_config
        .dpi_range
        .as_ref()
        .map(|r| r.step)
        .unwrap_or(100)
}

/* ---------------------------------------------------------------------- */
/* DeviceDriver trait implementation                                       */
/* ---------------------------------------------------------------------- */

#[async_trait]
impl DeviceDriver for SteelseriesDriver {
    fn name(&self) -> &str {
        "SteelSeries"
    }

    async fn probe(&mut self, _io: &mut DeviceIo) -> Result<()> {
        debug!("Probe called for SteelSeries");
        Ok(())
    }

    async fn load_profiles(&mut self, io: &mut DeviceIo, info: &mut DeviceInfo) -> Result<()> {
        /* Reject invalid protocol state up front: driving a device with
         * guessed V1 opcodes it may not speak is worse than not driving it.
         * Every shipped steelseries .device file sets DeviceVersion. */
        let raw = info.driver_config.device_version.ok_or_else(|| {
            anyhow!("SteelSeries: DeviceVersion missing from device database entry")
        })?;
        let version = ProtocolVersion::try_from(raw)?;
        self.version = Some(version);
        self.quirks = DeviceQuirks::from_config(&info.driver_config);

        let button_count = info.driver_config.buttons.unwrap_or(0);
        let led_count = info.driver_config.leds.unwrap_or(0);
        let senseiraw = self.quirks.is_senseiraw;

        /* Build the DPI list from the range specification if available. */
        let dpi_list: Vec<u32> = info
            .driver_config
            .dpi_range
            .as_ref()
            .map(|r| (r.min..=r.max).step_by(r.step as usize).collect())
            .unwrap_or_default();

        let report_rates = vec![125, 250, 500, 1000];

        info.profiles.clear();
        for profile_id in 0..STEELSERIES_NUM_PROFILES as u32 {
            let resolutions = (0..STEELSERIES_NUM_DPI as u32)
                .map(|res_id| ResolutionInfo {
                    index: res_id,
                    is_active: res_id == 0,
                    is_default: res_id == 0,
                    dpi: Dpi::Unified(800 * (res_id + 1)),
                    dpi_list: dpi_list.clone(),
                    capabilities: vec![],
                    is_disabled: false,
                })
                .collect();

            let buttons = (0..button_count)
                .map(|btn_id| build_button(btn_id, button_count, senseiraw))
                .collect();

            let leds = (0..led_count)
                .map(|led_id| build_led(version, led_id, senseiraw))
                .collect();

            let mut profile = ProfileInfo {
                index: profile_id,
                name: format!("Profile {profile_id}"),
                is_active: true,
                is_enabled: true,
                is_dirty: false,
                report_rate: 1000,
                report_rates: report_rates.clone(),
                angle_snapping: -1,
                debounce: -1,
                debounces: vec![],
                capabilities: vec![],
                resolutions,
                buttons,
                leds,
            };

            /* Attempt to override defaults by reading active hardware settings. */
            if let Err(e) = self.read_settings(io, &mut profile).await {
                warn!("SteelSeries: failed to read hardware settings: {e}");
            }

            info.profiles.push(profile);
        }

        if let Ok(fw) = self.read_firmware_version(io).await {
            info.firmware_version = fw;
        }

        Ok(())
    }

    async fn commit(&mut self, io: &mut DeviceIo, info: &DeviceInfo) -> Result<()> {
        let profile = info
            .profiles
            .iter()
            .find(|p| p.is_active)
            .or_else(|| info.profiles.first())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No profiles found in DeviceInfo (SteelSeries hardware requires at least 1)"
                )
            })?;

        if let Some(res) = profile.resolutions.iter().find(|r| r.is_active) {
            self.write_dpi(io, res, info).await?;
        }

        self.write_buttons(io, profile, info).await?;

        for led in &profile.leds {
            self.write_led(io, led).await?;
        }

        self.write_report_rate(io, profile.report_rate).await?;

        /* Persist everything to the device's EEPROM. */
        self.write_save(io).await?;

        Ok(())
    }
}

/* ---------------------------------------------------------------------- */
/* Default profile construction                                           */
/* ---------------------------------------------------------------------- */

/* Build a button with its default action, mirroring the C driver's
 * button_defaults_for_layout(): the resolution-cycle-up special lands on
 * button 5 for layouts of <=6 buttons, button 6 for 7, button 7 for 8+. */
fn build_button(btn_id: u32, button_count: u32, senseiraw: bool) -> ButtonInfo {
    let mut action_types = vec![
        ActionType::None as u32,
        ActionType::Button as u32,
        ActionType::Special as u32,
    ];
    if !senseiraw {
        action_types.push(ActionType::Macro as u32);
    }

    let special_idx = if button_count <= 6 {
        5
    } else if button_count == 7 {
        6
    } else {
        7
    };

    let (action_type, mapping_value) = if btn_id == special_idx {
        (ActionType::Special, special_action::RESOLUTION_CYCLE_UP)
    } else if btn_id < 8 {
        /* Regular mouse button (1-indexed for DBus compatibility). */
        (ActionType::Button, btn_id + 1)
    } else {
        (ActionType::None, 0)
    };

    ButtonInfo {
        index: btn_id,
        action_type,
        action_types,
        mapping_value,
        macro_entries: vec![],
    }
}

fn build_led(version: ProtocolVersion, led_id: u32, senseiraw: bool) -> LedInfo {
    /* V1 devices support Off, Solid, Breathing; V2+ add Cycle. */
    let mut modes = vec![LedMode::Off, LedMode::Solid, LedMode::Breathing];
    if matches!(
        version,
        ProtocolVersion::V2 | ProtocolVersion::V3 | ProtocolVersion::V4
    ) {
        modes.push(LedMode::Cycle);
    }

    let (color_depth, color, brightness) = if senseiraw {
        /* Monochrome – brightness controls intensity */
        (1, Color::default(), 255)
    } else {
        /* RGB_888 – default to blue as in the C driver */
        (3, Color { red: 0, green: 0, blue: 255 }, 255)
    };

    LedInfo {
        index: led_id,
        mode: LedMode::Solid,
        modes,
        color,
        secondary_color: Color::default(),
        tertiary_color: Color::default(),
        color_depth,
        effect_duration: 1000,
        brightness,
    }
}

/* ---------------------------------------------------------------------- */
/* Report dispatch                                                        */
/* ---------------------------------------------------------------------- */

impl SteelseriesDriver {
    /* Pause for the hardware to settle, then send `bytes` on the correct HID
     * channel. The single choke point for every command write. */
    async fn dispatch(
        &self,
        io: &mut DeviceIo,
        feature: bool,
        bytes: &[u8],
        opcode: u8,
        delay: Duration,
    ) -> Result<()> {
        tokio::time::sleep(delay).await;
        if feature {
            io.set_feature_report(bytes)
                .with_context(|| format!("SteelSeries feature report 0x{opcode:02x} failed"))?;
        } else {
            io.write_report(bytes)
                .await
                .with_context(|| format!("SteelSeries output report 0x{opcode:02x} failed"))?;
        }
        Ok(())
    }

    /* Send a fully-built report after the standard settle delay. */
    async fn send(&self, io: &mut DeviceIo, report: &Report) -> Result<()> {
        self.dispatch(io, report.feature, report.bytes(), report.opcode, SETTLE)
            .await
    }
}

/* ---------------------------------------------------------------------- */
/* Command writers                                                        */
/* ---------------------------------------------------------------------- */

impl SteelseriesDriver {
    async fn write_dpi(&self, io: &mut DeviceIo, res: &ResolutionInfo, info: &DeviceInfo) -> Result<()> {
        let dpi_val = match res.dpi {
            Dpi::Unified(d) => d,
            Dpi::Separate { x, .. } => x,
            Dpi::Unknown => 800,
        };
        let step = dpi_step(info);
        let res_id = res.index as u8 + 1;
        /* Hardware stores (dpi / step) - 1. */
        let scaled = (dpi_val / step).saturating_sub(1) as u8;

        let report = match self.version()? {
            ProtocolVersion::V1 => {
                /* V1 with an explicit DPI list reverse-looks up the index
                 * (the C driver enumerates entries in reverse). */
                let scaled = if res.dpi_list.is_empty() {
                    scaled
                } else {
                    let pos = res.dpi_list.iter().position(|&d| d == dpi_val).unwrap_or(0);
                    (res.dpi_list.len() - pos) as u8
                };
                let mut r = Report::output(STEELSERIES_ID_DPI_SHORT, STEELSERIES_REPORT_SIZE_SHORT);
                r.param(1, res_id).param(2, scaled);
                r
            }
            ProtocolVersion::V2 => {
                let mut r = Report::output(STEELSERIES_ID_DPI, STEELSERIES_REPORT_SIZE);
                r.param(2, res_id)
                    .param(3, scaled)
                    .param(6, STEELSERIES_DPI_MAGIC_MARKER);
                r
            }
            ProtocolVersion::V3 => {
                let mut r = Report::output(STEELSERIES_ID_DPI_PROTOCOL3, STEELSERIES_REPORT_SIZE);
                r.param(2, res_id)
                    .param(3, scaled)
                    .param(5, STEELSERIES_DPI_MAGIC_MARKER);
                r
            }
            ProtocolVersion::V4 => {
                /* V4 uses the 64-byte report, not SHORT. */
                let mut r = Report::output(STEELSERIES_ID_DPI_PROTOCOL4, STEELSERIES_REPORT_SIZE);
                r.param(1, res_id).param(2, scaled);
                r
            }
        };

        self.send(io, &report).await
    }

    async fn write_report_rate(&self, io: &mut DeviceIo, hz: u32) -> Result<()> {
        let version = self.version()?;
        let report = match version {
            ProtocolVersion::V1 | ProtocolVersion::V4 => {
                /* Discretized rate codes: 1000→0x01, 500→0x02, 250→0x03, 125→0x04. */
                let rate_code: u8 = if hz >= 1000 {
                    0x01
                } else if hz >= 375 {
                    0x02
                } else if hz <= 125 {
                    0x04
                } else {
                    0x03
                };
                let opcode = if version == ProtocolVersion::V1 {
                    STEELSERIES_ID_REPORT_RATE_SHORT
                } else {
                    STEELSERIES_ID_REPORT_RATE_PROTOCOL4
                };
                let mut r = Report::output(opcode, STEELSERIES_REPORT_SIZE_SHORT);
                r.param(2, rate_code);
                r
            }
            ProtocolVersion::V2 | ProtocolVersion::V3 => {
                let rate_val = (1000 / hz.max(125)) as u8;
                let opcode = if version == ProtocolVersion::V2 {
                    STEELSERIES_ID_REPORT_RATE
                } else {
                    STEELSERIES_ID_REPORT_RATE_PROTOCOL3
                };
                let mut r = Report::output(opcode, STEELSERIES_REPORT_SIZE);
                r.param(2, rate_val);
                r
            }
        };

        self.send(io, &report).await
    }

    async fn write_buttons(&self, io: &mut DeviceIo, profile: &ProfileInfo, info: &DeviceInfo) -> Result<()> {
        /* A reported macro length of zero means button writes are unsupported. */
        if info.driver_config.macro_length == Some(0) {
            return Ok(());
        }

        let senseiraw = self.quirks.is_senseiraw;
        let button_size = if senseiraw {
            STEELSERIES_BUTTON_SIZE_SENSEIRAW
        } else {
            STEELSERIES_BUTTON_SIZE_STANDARD
        };
        let report_size = if senseiraw {
            STEELSERIES_REPORT_SIZE_SHORT
        } else {
            STEELSERIES_REPORT_LONG_SIZE
        };
        let max_modifiers: usize = if senseiraw { 0 } else { 3 };

        let mut report = Report::output(STEELSERIES_ID_BUTTONS, report_size);
        let buf = report.body_mut();

        for button in &profile.buttons {
            /* Each button occupies `button_size` bytes from parameters[2]
             * onward, i.e. output-report offset 3. */
            let idx = 3 + (button.index as usize) * button_size;
            if idx >= report_size {
                continue;
            }

            match button.action_type {
                ActionType::Button => {
                    buf[idx] = button.mapping_value as u8;
                }
                ActionType::Key | ActionType::Macro => {
                    pack_key_button(buf, idx, report_size, button, senseiraw, max_modifiers);
                }
                ActionType::Special => {
                    buf[idx] = match button.mapping_value {
                        special_action::RESOLUTION_CYCLE_UP => STEELSERIES_BUTTON_RES_CYCLE,
                        special_action::WHEEL_UP => STEELSERIES_BUTTON_WHEEL_UP,
                        special_action::WHEEL_DOWN => STEELSERIES_BUTTON_WHEEL_DOWN,
                        _ => STEELSERIES_BUTTON_OFF,
                    };
                }
                _ => buf[idx] = STEELSERIES_BUTTON_OFF,
            }
        }

        /* V3 carries buttons as a feature report: drop the report-id byte so
         * the opcode becomes the feature report number. */
        if self.version()? == ProtocolVersion::V3 {
            self.dispatch(io, true, &report.bytes()[1..report_size], STEELSERIES_ID_BUTTONS, SETTLE)
                .await
        } else {
            self.dispatch(io, false, report.bytes(), STEELSERIES_ID_BUTTONS, SETTLE)
                .await
        }
    }

    async fn write_save(&self, io: &mut DeviceIo) -> Result<()> {
        let (opcode, len) = match self.version()? {
            ProtocolVersion::V1 => (STEELSERIES_ID_SAVE_SHORT, STEELSERIES_REPORT_SIZE_SHORT),
            ProtocolVersion::V2 => (STEELSERIES_ID_SAVE, STEELSERIES_REPORT_SIZE),
            ProtocolVersion::V3 | ProtocolVersion::V4 => {
                (STEELSERIES_ID_SAVE_PROTOCOL3, STEELSERIES_REPORT_SIZE)
            }
        };
        let report = Report::output(opcode, len);
        self.dispatch(io, false, report.bytes(), opcode, SETTLE_SAVE)
            .await
    }

    /* ------------------------------------------------------------------ */
    /* LEDs                                                                */
    /* ------------------------------------------------------------------ */

    async fn write_led(&self, io: &mut DeviceIo, led: &LedInfo) -> Result<()> {
        match self.version()? {
            ProtocolVersion::V1 => self.write_led_v1(io, led).await,
            ProtocolVersion::V2 => self.write_led_v2(io, led).await,
            ProtocolVersion::V3 => self.write_led_v3(io, led).await,
            /* No V4 LED command exists in the protocol (C driver parity);
             * the only V4 device (Rival 650) declares Leds=0, so reaching
             * this arm means the device database entry is wrong. */
            ProtocolVersion::V4 => bail!("SteelSeries V4 has no LED protocol"),
        }
    }

    /* V1: a separate effect report followed by a color/intensity report.
     * Rival100 and SenseiRaw both deviate (color opcode / monochrome). */
    async fn write_led_v1(&self, io: &mut DeviceIo, led: &LedInfo) -> Result<()> {
        let rival100 = self.quirks.is_rival100;
        let senseiraw = self.quirks.is_senseiraw;

        let effect = match led.mode {
            LedMode::Off | LedMode::Solid => 0x01,
            LedMode::Breathing => {
                let ms = led.effect_duration;
                if ms <= 3000 {
                    0x04
                } else if ms <= 5000 {
                    0x03
                } else {
                    0x02
                }
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "SteelSeries V1: unsupported LED mode {:?}",
                    led.mode
                ));
            }
        };

        let mut effect_report =
            Report::output(STEELSERIES_ID_LED_EFFECT_SHORT, STEELSERIES_REPORT_SIZE_SHORT);
        effect_report
            .param(1, if rival100 { 0x00 } else { led.index as u8 + 1 })
            .param(2, effect);
        self.send(io, &effect_report).await?;

        /* Second report: color (RGB) or intensity (monochrome). */
        let color_report = if senseiraw {
            let intensity = if led.mode == LedMode::Off || led.brightness == 0 {
                1
            } else {
                /* Split brightness into ~3 bands: 0-85→2, 86-171→3, 172-255→4. */
                (led.brightness as u8 / 86) + 2
            };
            let mut r =
                Report::output(STEELSERIES_ID_LED_INTENSITY_SHORT, STEELSERIES_REPORT_SIZE_SHORT);
            r.param(1, led.index as u8 + 1).param(2, intensity);
            r
        } else if rival100 {
            /* Rival100 uses a different color opcode and a fixed led id of 0. */
            let mut r = Report::output(
                STEELSERIES_ID_LED_COLOR_SHORT_RIVAL100,
                STEELSERIES_REPORT_SIZE_SHORT,
            );
            r.param(1, 0x00)
                .param(2, led.color.red as u8)
                .param(3, led.color.green as u8)
                .param(4, led.color.blue as u8);
            r
        } else {
            let mut r = Report::output(STEELSERIES_ID_LED_COLOR_SHORT, STEELSERIES_REPORT_SIZE_SHORT);
            r.param(1, led.index as u8 + 1)
                .param(2, led.color.red as u8)
                .param(3, led.color.green as u8)
                .param(4, led.color.blue as u8);
            r
        };

        self.send(io, &color_report).await
    }

    /* V2 cycle buffer (C steelseries_led_cycle_spec, V2 layout):
     *   led_id   → parameters[2]
     *   duration → parameters[3..5]  (u16 LE)
     *   repeat   → parameters[19]
     *   npoints  → parameters[27]
     *   color data starts at parameters[28] (buf offset 29). */
    async fn write_led_v2(&self, io: &mut DeviceIo, led: &LedInfo) -> Result<()> {
        let mut report = Report::output(STEELSERIES_ID_LED, STEELSERIES_REPORT_SIZE);
        report.param(2, led.index as u8);

        let (repeat, points, duration) = build_cycle_points(led);
        if !repeat {
            report.param(19, 0x01);
        }

        let npoints = write_cycle_points(report.body_mut(), 29, &points);
        report.param(27, npoints);
        let d = (npoints as u16 * 330).max(duration);
        report.param_u16_le(3, d);

        self.send(io, &report).await
    }

    /* V3 cycle buffer (C steelseries_led_cycle_spec, V3 layout); sent as a HID
     * feature report so the opcode is the report number:
     *   led_id  → parameters[2], duplicated at parameters[7]
     *   duration → parameters[8..10]  (u16 LE)
     *   repeat   → parameters[24]
     *   npoints  → parameters[29]
     *   color data starts at parameters[30] (buf offset 30). */
    async fn write_led_v3(&self, io: &mut DeviceIo, led: &LedInfo) -> Result<()> {
        let mut report = Report::feature(STEELSERIES_ID_LED_PROTOCOL3, STEELSERIES_REPORT_SIZE);
        report.param(2, led.index as u8).param(7, led.index as u8);

        let (repeat, points, duration) = build_cycle_points(led);
        if !repeat {
            report.param(24, 0x01);
        }

        let npoints = write_cycle_points(report.body_mut(), 30, &points);
        report.param(29, npoints);
        let d = (npoints as u16 * 330).max(duration);
        report.param_u16_le(8, d);

        self.send(io, &report).await
    }

    /* ------------------------------------------------------------------ */
    /* Hardware reads                                                      */
    /* ------------------------------------------------------------------ */

    async fn read_firmware_version(&self, io: &mut DeviceIo) -> Result<String> {
        let (opcode, len) = match self.version()? {
            ProtocolVersion::V1 => {
                (STEELSERIES_ID_FIRMWARE_PROTOCOL1, STEELSERIES_REPORT_SIZE_SHORT)
            }
            ProtocolVersion::V2 => (STEELSERIES_ID_FIRMWARE_PROTOCOL2, STEELSERIES_REPORT_SIZE),
            ProtocolVersion::V3 => (STEELSERIES_ID_FIRMWARE_PROTOCOL3, STEELSERIES_REPORT_SIZE),
            /* V4 exposes no firmware query (C driver parity): report an
             * empty version rather than probing with a foreign opcode. */
            ProtocolVersion::V4 => return Ok(String::new()),
        };

        self.send(io, &Report::output(opcode, len)).await?;

        /* Best-effort read: some variants are write-only for this report. */
        let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
        if let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(500), io.read_report(&mut buf)).await
        {
            if n >= 2 {
                let major = buf.get(1).copied().unwrap_or(0);
                let minor = buf.get(0).copied().unwrap_or(0);
                return Ok(format!("{major}.{minor}"));
            }
        }

        Ok(String::new())
    }

    async fn read_settings(&self, io: &mut DeviceIo, profile: &mut ProfileInfo) -> Result<()> {
        let version = self.version()?;
        let settings_id = match version {
            ProtocolVersion::V2 => STEELSERIES_ID_SETTINGS,
            ProtocolVersion::V3 => STEELSERIES_ID_SETTINGS_PROTOCOL3,
            /* V1 and V4 have no settings-read command (C driver parity):
             * the defaults seeded by load_profiles stand. */
            ProtocolVersion::V1 | ProtocolVersion::V4 => return Ok(()),
        };

        self.send(io, &Report::output(settings_id, STEELSERIES_REPORT_SIZE))
            .await?;

        let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
        let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(500), io.read_report(&mut buf)).await
        else {
            return Ok(());
        };
        if n < 2 {
            return Ok(());
        }

        match version {
            ProtocolVersion::V2 => {
                let active_resolution = buf.get(1).copied().unwrap_or(0).saturating_sub(1);
                for res in &mut profile.resolutions {
                    res.is_active = res.index == active_resolution as u32;
                    let dpi_idx = 2 + res.index as usize * 2;
                    if dpi_idx < n {
                        let dpi_val = 100 * (1 + buf.get(dpi_idx).copied().unwrap_or(0) as u32);
                        res.dpi = Dpi::Unified(dpi_val);
                    }
                }

                for led in &mut profile.leds {
                    let offset = 6 + led.index as usize * 3;
                    if offset + 2 < n {
                        led.color.red = buf.get(offset).copied().unwrap_or(0) as u32;
                        led.color.green = buf.get(offset + 1).copied().unwrap_or(0) as u32;
                        led.color.blue = buf.get(offset + 2).copied().unwrap_or(0) as u32;
                    }
                }
            }
            ProtocolVersion::V3 => {
                let active_resolution = buf.get(0).copied().unwrap_or(0).saturating_sub(1);
                for res in &mut profile.resolutions {
                    res.is_active = res.index == active_resolution as u32;
                }
            }
            /* Unreachable: returned above before any I/O. */
            ProtocolVersion::V1 | ProtocolVersion::V4 => {}
        }

        Ok(())
    }
}

/* ---------------------------------------------------------------------- */
/* Button key/modifier packing                                            */
/* ---------------------------------------------------------------------- */

/* HID modifier usage codes, indexed by their bit in the SteelSeries modifier
 * mask (bit 0 = LCTRL … bit 7 = RMETA). */
const MODIFIER_USAGE: [u8; 8] = [0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7];

/* Pack a Key/Macro button binding into `buf` at offset `idx`.  Resolves the
 * modifier mask and final keycode from the macro entries (or the raw mapping
 * value), then emits the layout's keyboard/consumer encoding. */
fn pack_key_button(
    buf: &mut [u8],
    idx: usize,
    report_size: usize,
    button: &ButtonInfo,
    senseiraw: bool,
    max_modifiers: usize,
) {
    let mut modifiers = 0u8;
    let mut final_key = 0u8;

    for &(ev_type, k) in &button.macro_entries {
        if ev_type == 0 {
            /* Key press. HID usages 224..=231 are the eight modifiers. */
            match k {
                224..=231 => modifiers |= 1 << (k - 224),
                _ => final_key = (k % 256) as u8,
            }
        }
    }

    if button.macro_entries.is_empty() {
        final_key = (button.mapping_value % 256) as u8;
    }

    if modifiers.count_ones() as usize > max_modifiers {
        warn!(
            "SteelSeries: button {} has too many modifiers ({}, max {})",
            button.index,
            modifiers.count_ones(),
            max_modifiers
        );
    }

    if final_key == 0 {
        /* No keyboard usage: treat as a consumer-control binding. */
        buf[idx] = STEELSERIES_BUTTON_CONSUMER;
        if idx + 1 < report_size {
            buf[idx + 1] = (button.mapping_value % 256) as u8;
        }
        return;
    }

    if senseiraw {
        buf[idx] = STEELSERIES_BUTTON_KEY;
        if idx + 1 < report_size {
            buf[idx + 1] = final_key;
        }
        return;
    }

    /* Standard keyboard: opcode, up to `max_modifiers` modifier usages, key. */
    buf[idx] = STEELSERIES_BUTTON_KBD;
    let mut cursor = idx;
    for (bit, &usage) in MODIFIER_USAGE.iter().enumerate() {
        if (modifiers & (1 << bit)) != 0 && cursor - idx < max_modifiers {
            if cursor + 1 < report_size {
                buf[cursor + 1] = usage;
            }
            cursor += 1;
        }
    }
    if cursor + 1 < report_size {
        buf[cursor + 1] = final_key;
    }
}

/* ---------------------------------------------------------------------- */
/* Cycle-point construction (shared between V2 and V3)                    */
/* ---------------------------------------------------------------------- */

/* A single color-position point in a LED cycle animation. */
struct CyclePoint {
    r: u8,
    g: u8,
    b: u8,
    pos: u8,
}

impl CyclePoint {
    const fn new(r: u8, g: u8, b: u8, pos: u8) -> Self {
        Self { r, g, b, pos }
    }

    fn solid(color: &Color, pos: u8) -> Self {
        Self::new(color.red as u8, color.green as u8, color.blue as u8, pos)
    }
}

/* Build the cycle control points for a LED mode.  Returns (repeat, points,
 * duration_ms). */
fn build_cycle_points(led: &LedInfo) -> (bool, Vec<CyclePoint>, u16) {
    match led.mode {
        LedMode::Off => (false, vec![CyclePoint::new(0, 0, 0, 0x00)], 5000),
        LedMode::Solid => (false, vec![CyclePoint::solid(&led.color, 0x00)], 5000),
        LedMode::Cycle => {
            /* 4-point rainbow: red → green → blue → red. */
            let points = vec![
                CyclePoint::new(0xFF, 0x00, 0x00, 0x00),
                CyclePoint::new(0x00, 0xFF, 0x00, 0x55),
                CyclePoint::new(0x00, 0x00, 0xFF, 0x55),
                CyclePoint::new(0xFF, 0x00, 0x00, 0x55),
            ];
            (true, points, led.effect_duration as u16)
        }
        LedMode::Breathing => {
            /* 3-point breathe: black → color → black. */
            let points = vec![
                CyclePoint::new(0, 0, 0, 0x00),
                CyclePoint::solid(&led.color, 0x7F),
                CyclePoint::new(0, 0, 0, 0x7F),
            ];
            (true, points, led.effect_duration as u16)
        }
        _ => (false, vec![CyclePoint::new(0, 0, 0, 0x00)], 5000),
    }
}

/* Write cycle points into `buf` following the C construct_cycle_buffer()
 * layout: the first point's color is duplicated as a 3-byte RGB header
 * immediately before the regular 4-byte (r,g,b,pos) point array.
 * Returns the number of points written. */
fn write_cycle_points(buf: &mut [u8], header_start: usize, points: &[CyclePoint]) -> u8 {
    let mut color_idx = header_start;

    for (i, pt) in points.iter().enumerate() {
        if i == 0 {
            /* Write the first point's color as a 3-byte header. */
            buf[color_idx] = pt.r;
            buf[color_idx + 1] = pt.g;
            buf[color_idx + 2] = pt.b;
            color_idx += 3;
        }

        let base = color_idx + i * 4;
        if base + 3 < buf.len() {
            buf[base] = pt.r;
            buf[base + 1] = pt.g;
            buf[base + 2] = pt.b;
            buf[base + 3] = pt.pos;
        }
    }

    points.len() as u8
}

/* ---------------------------------------------------------------------- */
/* Tests                                                                  */
/* ---------------------------------------------------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::device_database::{DeviceEntry, DpiRange, DriverConfig};
    use crate::hal::mock::MockExchange;

    fn make_info(device_version: Option<u32>, quirks: &[&str]) -> DeviceInfo {
        let entry = DeviceEntry {
            name: "Test Mouse".into(),
            driver: "steelseries".into(),
            device_type: "mouse".into(),
            matches: Vec::new(),
            driver_config: Some(DriverConfig {
                buttons: Some(6),
                leds: Some(1),
                device_version,
                dpi_range: Some(DpiRange { min: 100, max: 12000, step: 100 }),
                quirks: quirks.iter().map(|s| s.to_string()).collect(),
                ..DriverConfig::default()
            }),
        };
        DeviceInfo::from_entry("test0", "Test Mouse", 0x03, 0x1038, 0x1702, &entry)
    }

    #[test]
    fn protocol_version_parses_valid_range() {
        assert_eq!(ProtocolVersion::try_from(1).unwrap(), ProtocolVersion::V1);
        assert_eq!(ProtocolVersion::try_from(2).unwrap(), ProtocolVersion::V2);
        assert_eq!(ProtocolVersion::try_from(3).unwrap(), ProtocolVersion::V3);
        assert_eq!(ProtocolVersion::try_from(4).unwrap(), ProtocolVersion::V4);
    }

    #[test]
    fn protocol_version_rejects_out_of_range() {
        assert!(ProtocolVersion::try_from(0).is_err());
        assert!(ProtocolVersion::try_from(5).is_err());
    }

    #[tokio::test]
    async fn load_profiles_rejects_missing_device_version() {
        let (mut io, handle) = DeviceIo::with_mock(Vec::new());
        let mut info = make_info(None, &[]);
        let mut drv = SteelseriesDriver::new();

        let err = drv.load_profiles(&mut io, &mut info).await.unwrap_err();
        assert!(err.to_string().contains("DeviceVersion missing"));
        assert!(drv.version.is_none());
        assert!(handle.writes().is_empty(), "must fail before any I/O");
    }

    #[tokio::test]
    async fn load_profiles_rejects_invalid_device_version() {
        let (mut io, handle) = DeviceIo::with_mock(Vec::new());
        let mut info = make_info(Some(7), &[]);
        let mut drv = SteelseriesDriver::new();

        let err = drv.load_profiles(&mut io, &mut info).await.unwrap_err();
        assert!(err.to_string().contains("unsupported DeviceVersion 7"));
        assert!(drv.version.is_none());
        assert!(handle.writes().is_empty(), "must fail before any I/O");
    }

    /* V1 end-to-end load: settings read is a documented no-op, so the only
     * wire traffic is the firmware query (write) and its reply. */
    #[tokio::test]
    async fn load_profiles_v1_seeds_state_and_reads_firmware() {
        let fw_request =
            Report::output(STEELSERIES_ID_FIRMWARE_PROTOCOL1, STEELSERIES_REPORT_SIZE_SHORT);
        let (mut io, handle) = DeviceIo::with_mock(vec![MockExchange::expect_reply(
            fw_request.bytes().to_vec(),
            vec![0x02, 0x01], /* minor, major -> "1.2" */
        )]);
        let mut info = make_info(Some(1), &[]);
        let mut drv = SteelseriesDriver::new();

        drv.load_profiles(&mut io, &mut info).await.unwrap();

        assert_eq!(drv.version, Some(ProtocolVersion::V1));
        assert_eq!(info.firmware_version, "1.2");
        assert_eq!(info.profiles.len(), 1);

        let profile = &info.profiles[0];
        assert_eq!(profile.resolutions.len(), STEELSERIES_NUM_DPI as usize);
        assert_eq!(profile.buttons.len(), 6);
        /* V1 LEDs must not advertise Cycle. */
        assert_eq!(
            profile.leds[0].modes,
            vec![LedMode::Off, LedMode::Solid, LedMode::Breathing]
        );
        assert!(handle.script_exhausted());
        /* No quirk strings configured -> both flags off. */
        assert_eq!(drv.quirks, DeviceQuirks::default());
    }

    #[test]
    fn device_quirks_parse_from_config() {
        let quirks = |strings: &[&str]| {
            DeviceQuirks::from_config(&DriverConfig {
                quirks: strings.iter().map(|s| s.to_string()).collect(),
                ..DriverConfig::default()
            })
        };

        assert_eq!(quirks(&[]), DeviceQuirks::default());
        assert_eq!(
            quirks(&["STEELSERIES_QUIRK_SENSEIRAW"]),
            DeviceQuirks { is_senseiraw: true, is_rival100: false }
        );
        assert_eq!(
            quirks(&["STEELSERIES_QUIRK_RIVAL100", "STEELSERIES_QUIRK_SENSEIRAW"]),
            DeviceQuirks { is_senseiraw: true, is_rival100: true }
        );
        /* Unknown quirk strings are ignored, not misparsed. */
        assert_eq!(quirks(&["SOME_OTHER_QUIRK"]), DeviceQuirks::default());
    }

    /* Quirks are parsed exactly once during load_profiles and shape the
     * seeded state (SenseiRaw: monochrome LED, no Macro action type). */
    #[tokio::test]
    async fn load_profiles_persists_quirks_in_driver_state() {
        let fw_request =
            Report::output(STEELSERIES_ID_FIRMWARE_PROTOCOL1, STEELSERIES_REPORT_SIZE_SHORT);
        let (mut io, _handle) = DeviceIo::with_mock(vec![MockExchange::expect_reply(
            fw_request.bytes().to_vec(),
            vec![0x00, 0x01],
        )]);
        let mut info = make_info(Some(1), &["STEELSERIES_QUIRK_SENSEIRAW"]);
        let mut drv = SteelseriesDriver::new();

        drv.load_profiles(&mut io, &mut info).await.unwrap();

        assert!(drv.quirks.is_senseiraw);
        assert!(!drv.quirks.is_rival100);

        let profile = &info.profiles[0];
        assert_eq!(profile.leds[0].color_depth, 1, "SenseiRaw LED is monochrome");
        assert!(
            !profile.buttons[0].action_types.contains(&(ActionType::Macro as u32)),
            "SenseiRaw buttons must not advertise Macro"
        );
    }
}
