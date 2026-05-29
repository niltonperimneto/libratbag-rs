/* OpenInput protocol driver.
 *
 * Targets mice implementing the OpenInput HID protocol, an open-source
 * hardware configuration protocol for gaming peripherals.
 *
 * Reference implementation: src/driver-openinput.c.
 */
use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::engine::device::DeviceInfo;
use crate::hal::{DeviceDriver, DeviceIo};

/* ------------------------------------------------------------------ */
/* Report IDs and sizes                                                 */
/* ------------------------------------------------------------------ */

/* Short report ID (8 bytes total). */
const OI_REPORT_SHORT: u8 = 0x20;
/* Long report ID (32 bytes total). */
const OI_REPORT_LONG: u8 = 0x21;

const OI_REPORT_SHORT_SIZE: usize = 8;
const OI_REPORT_LONG_SIZE: usize = 32;
const OI_REPORT_MAX_SIZE: usize = OI_REPORT_LONG_SIZE;
/* Byte offset where payload data begins inside a report. */
const OI_REPORT_DATA_INDEX: usize = 3;
const OI_REPORT_DATA_MAX_SIZE: usize = OI_REPORT_LONG_SIZE - OI_REPORT_DATA_INDEX;

/* ------------------------------------------------------------------ */
/* Protocol function pages                                              */
/* ------------------------------------------------------------------ */

const OI_PAGE_INFO: u8 = 0x00;
const OI_PAGE_GIMMICKS: u8 = 0xFD;
const OI_PAGE_DEBUG: u8 = 0xFE;
const OI_PAGE_ERROR: u8 = 0xFF;

/* Info page (0x00) functions */
const OI_FUNCTION_VERSION: u8 = 0x00;
const OI_FUNCTION_FW_INFO: u8 = 0x01;
const OI_FUNCTION_SUPPORTED_PAGES: u8 = 0x02;
const OI_FUNCTION_SUPPORTED_FUNCTIONS: u8 = 0x03;

/* Firmware info field IDs for OI_FUNCTION_FW_INFO */
const OI_FW_INFO_VENDOR: u8 = 0x00;
const OI_FW_INFO_VERSION: u8 = 0x01;
const OI_FW_INFO_DEVICE_NAME: u8 = 0x02;

/* Error page (0xFF) codes */
const OI_ERROR_INVALID_VALUE: u8 = 0x01;
const OI_ERROR_UNSUPPORTED_FUNCTION: u8 = 0x02;
const OI_ERROR_CUSTOM: u8 = 0xFE;

/* Valid polling rates reported to higher layers (Hz). */
const REPORT_RATES: &[u32] = &[125, 250, 500, 750, 1000];

/* ------------------------------------------------------------------ */
/* Report payload layout                                                */
/* ------------------------------------------------------------------ */

/* A packed OpenInput HID report. */
#[derive(Debug, Default, Clone)]
pub struct OiReport {
    /* Report ID (OI_REPORT_SHORT or OI_REPORT_LONG). */
    pub id: u8,
    /* Function page. */
    pub function_page: u8,
    /* Function number within the page. */
    pub function: u8,
    /* Payload bytes. */
    pub data: [u8; OI_REPORT_DATA_MAX_SIZE],
}

impl OiReport {
    /* Serialize into a short (8-byte) buffer. */
    pub fn to_short_buf(&self) -> [u8; OI_REPORT_SHORT_SIZE] {
        let mut buf = [0u8; OI_REPORT_SHORT_SIZE];
        buf[0] = self.id;
        buf[1] = self.function_page;
        buf[2] = self.function;
        let len = (OI_REPORT_SHORT_SIZE - OI_REPORT_DATA_INDEX).min(self.data.len());
        buf[OI_REPORT_DATA_INDEX..OI_REPORT_DATA_INDEX + len]
            .copy_from_slice(&self.data[..len]);
        buf
    }

    /* Serialize into a long (32-byte) buffer. */
    pub fn to_long_buf(&self) -> [u8; OI_REPORT_LONG_SIZE] {
        let mut buf = [0u8; OI_REPORT_LONG_SIZE];
        buf[0] = self.id;
        buf[1] = self.function_page;
        buf[2] = self.function;
        let len = OI_REPORT_DATA_MAX_SIZE.min(self.data.len());
        buf[OI_REPORT_DATA_INDEX..OI_REPORT_DATA_INDEX + len]
            .copy_from_slice(&self.data[..len]);
        buf
    }

    /* Deserialize from a raw buffer. The caller must guarantee `buf.len() >= 3`. */
    fn from_buf(buf: &[u8]) -> Self {
        let mut report = OiReport::default();
        report.id = buf[0];
        report.function_page = buf[1];
        report.function = buf[2];
        let data_len = buf.len().saturating_sub(OI_REPORT_DATA_INDEX);
        let copy_len = data_len.min(OI_REPORT_DATA_MAX_SIZE);
        report.data[..copy_len].copy_from_slice(&buf[OI_REPORT_DATA_INDEX..OI_REPORT_DATA_INDEX + copy_len]);
        report
    }
}

/* ------------------------------------------------------------------ */
/* Capability bitmask                                                   */
/* ------------------------------------------------------------------ */

/* Bitmask of supported feature pages discovered via SUPPORTED_PAGES. */
pub type SupportedPages = u64;

/* ------------------------------------------------------------------ */
/* Cached state                                                         */
/* ------------------------------------------------------------------ */

/* Fields num_resolutions, num_buttons, num_leds, and supported are
 * reserved for future use when the protocol gains DPI, button, and LED
 * configuration commands.  The capability discovery loop already populates
 * the infrastructure; the fields are intentionally unread for now. */
#[allow(dead_code)]
#[derive(Debug)]
struct OiData {
    fw_major: u8,
    fw_minor: u8,
    fw_patch: u8,
    num_profiles: u32,
    num_resolutions: u32,
    num_buttons: u32,
    num_leds: u32,
    supported: SupportedPages,
}

/* ------------------------------------------------------------------ */
/* Driver                                                               */
/* ------------------------------------------------------------------ */

pub struct OpenInputDriver {
    data: Option<OiData>,
}

impl OpenInputDriver {
    pub fn new() -> Self {
        Self { data: None }
    }

    /* ---- Core I/O primitive --------------------------------------- */

    /* Send a request report and receive the device's response.
     * This is the Rust equivalent of C's openinput_send_report().
     *
     * The method serialises the request using the appropriate buffer
     * size (8 bytes for SHORT, 32 for LONG), writes it, reads back a
     * response, validates the report ID and size, and returns a parsed
     * OiReport. If the device responds with an error page (0xFF), the
     * error code and payload are converted to a human-readable message
     * via format_error_report() and returned as Err. */
    async fn send_report(&self, io: &mut DeviceIo, report: OiReport) -> Result<OiReport> {
        let mut rx_buf = [0u8; OI_REPORT_MAX_SIZE];

        match report.id {
            OI_REPORT_SHORT => {
                let buf = report.to_short_buf();
                io.write_report(&buf).await.context("OpenInput: send_report write failed")?;
            }
            OI_REPORT_LONG => {
                let buf = report.to_long_buf();
                io.write_report(&buf).await.context("OpenInput: send_report write failed")?;
            }
            id => anyhow::bail!("OpenInput: unknown report ID 0x{id:02x}"),
        }

        let n = io.read_report(&mut rx_buf).await.context("OpenInput: send_report read failed")?;

        if n < OI_REPORT_DATA_INDEX {
            anyhow::bail!("OpenInput: response too short ({n} bytes)");
        }

        /* Validate the response report ID and check size matches. */
        let expected_size = match rx_buf[0] {
            OI_REPORT_SHORT => OI_REPORT_SHORT_SIZE,
            OI_REPORT_LONG => OI_REPORT_LONG_SIZE,
            id => anyhow::bail!("OpenInput: unexpected response report ID 0x{id:02x}"),
        };
        if n != expected_size {
            anyhow::bail!(
                "OpenInput: response size mismatch (got {n}, expected {expected_size})"
            );
        }

        let response = OiReport::from_buf(&rx_buf[..n]);

        /* If the device returned an error page, translate and propagate it. */
        if response.function_page == OI_PAGE_ERROR {
            anyhow::bail!("{}", Self::format_error_report(&response));
        }

        Ok(response)
    }

    /* Translate an error-page response into a human-readable string.
     * Mirrors C's openinput_get_error_string(). */
    fn format_error_report(report: &OiReport) -> String {
        match report.function {
            OI_ERROR_INVALID_VALUE => {
                format!("OpenInput device error: Invalid value (in position {})", report.data[2])
            }
            OI_ERROR_UNSUPPORTED_FUNCTION => {
                format!(
                    "OpenInput device error: Unsupported function (0x{:02x}, 0x{:02x})",
                    report.data[0], report.data[1]
                )
            }
            OI_ERROR_CUSTOM => {
                /* Custom error: data bytes contain a NUL-terminated ASCII string. */
                let end = report.data.iter().position(|&b| b == 0).unwrap_or(report.data.len());
                let msg = String::from_utf8_lossy(&report.data[..end]);
                format!("OpenInput device error: Custom error ({msg})")
            }
            code => format!("OpenInput device error: Unknown error (0x{code:02x})"),
        }
    }

    /* ---- Info page helpers ---------------------------------------- */

    /* Query OI_FUNCTION_VERSION and store major/minor/patch in self.data. */
    async fn info_version(&mut self, io: &mut DeviceIo) -> Result<()> {
        let req = OiReport {
            id: OI_REPORT_SHORT,
            function_page: OI_PAGE_INFO,
            function: OI_FUNCTION_VERSION,
            data: [0u8; OI_REPORT_DATA_MAX_SIZE],
        };
        let resp = self.send_report(io, req).await
            .context("OpenInput: version query failed")?;

        let major = resp.data[0];
        let minor = resp.data[1];
        let patch = resp.data[2];

        info!("OpenInput: protocol version {major}.{minor}.{patch}");

        if let Some(d) = self.data.as_mut() {
            d.fw_major = major;
            d.fw_minor = minor;
            d.fw_patch = patch;
        }
        Ok(())
    }

    /* Query OI_FUNCTION_FW_INFO for a given field_id.
     * Returns the response data as a UTF-8 lossy string (NUL-terminated). */
    async fn info_fw_info(&self, io: &mut DeviceIo, field_id: u8) -> Result<String> {
        let mut data = [0u8; OI_REPORT_DATA_MAX_SIZE];
        data[0] = field_id;
        let req = OiReport {
            id: OI_REPORT_SHORT,
            function_page: OI_PAGE_INFO,
            function: OI_FUNCTION_FW_INFO,
            data,
        };
        let resp = self.send_report(io, req).await
            .context("OpenInput: fw_info query failed")?;

        /* The data field contains a NUL-terminated ASCII string. */
        let end = resp.data.iter().position(|&b| b == 0).unwrap_or(resp.data.len());
        Ok(String::from_utf8_lossy(&resp.data[..end]).into_owned())
    }

    /* Query supported function pages with pagination (start_index).
     * Returns (count_in_batch, left_remaining, page_list). */
    async fn info_supported_function_pages(
        &self, io: &mut DeviceIo, start_index: u8,
    ) -> Result<(u8, u8, Vec<u8>)> {
        let mut data = [0u8; OI_REPORT_DATA_MAX_SIZE];
        data[0] = start_index;
        let req = OiReport {
            id: OI_REPORT_SHORT,
            function_page: OI_PAGE_INFO,
            function: OI_FUNCTION_SUPPORTED_PAGES,
            data,
        };
        let resp = self.send_report(io, req).await
            .context("OpenInput: supported_function_pages query failed")?;

        let count = resp.data[0];
        let left = resp.data[1];
        let pages = resp.data[2..2 + usize::from(count)].to_vec();
        Ok((count, left, pages))
    }

    /* Query supported functions within a page with pagination.
     * Returns (count_in_batch, left_remaining, function_list). */
    async fn info_supported_functions(
        &self, io: &mut DeviceIo, function_page: u8, start_index: u8,
    ) -> Result<(u8, u8, Vec<u8>)> {
        let mut data = [0u8; OI_REPORT_DATA_MAX_SIZE];
        data[0] = function_page;
        data[1] = start_index;
        let req = OiReport {
            id: OI_REPORT_SHORT,
            function_page: OI_PAGE_INFO,
            function: OI_FUNCTION_SUPPORTED_FUNCTIONS,
            data,
        };
        let resp = self.send_report(io, req).await
            .context("OpenInput: supported_functions query failed")?;

        let count = resp.data[0];
        let left = resp.data[1];
        let funcs = resp.data[2..2 + usize::from(count)].to_vec();
        Ok((count, left, funcs))
    }

    /* Enumerate all functions supported within a single function page.
     * Loops with pagination until `left == 0`, validating that the total
     * remains consistent each iteration to prevent infinite loops (the same
     * deadlock guard as C's `total != (read + count + left)` check). */
    async fn read_supported_functions(&mut self, io: &mut DeviceIo, page: u8) -> Result<()> {
        let (count, left, first_batch) =
            self.info_supported_functions(io, page, 0).await?;

        let total = usize::from(count) + usize::from(left);
        let mut functions: Vec<u8> = Vec::with_capacity(total);
        functions.extend_from_slice(&first_batch);
        let mut read = usize::from(count);
        let mut remaining = left;

        while remaining > 0 {
            let (c, l, batch) =
                self.info_supported_functions(io, page, read as u8).await?;

            /* Guard against inconsistent pagination responses to prevent
             * deadlocks — mirrors the `-EINVAL` path in C. */
            if total != read + usize::from(c) + usize::from(l) {
                anyhow::bail!(
                    "OpenInput: invalid number of functions left to read ({l}) \
                     on page 0x{page:02x}"
                );
            }
            debug!("OpenInput: read {c} functions, {l} left on page {}", page_name(page));
            functions.extend_from_slice(&batch);
            read += usize::from(c);
            remaining = l;
        }

        for func in &functions {
            debug!(
                "OpenInput: found function 0x{:02x} 0x{:02x} on page {}",
                page, func, page_name(page)
            );
            /* TODO: set bits in self.data.supported when specific
             * capabilities (DPI, button remapping, LED control) are
             * implemented and can be acted upon. */
        }

        Ok(())
    }

    /* Enumerate all supported function pages, then query each page's
     * functions.  Mirrors C's openinput_read_supported_function_pages(). */
    async fn read_supported_function_pages(&mut self, io: &mut DeviceIo) -> Result<()> {
        debug!("OpenInput: starting device function enumeration");

        let (count, left, first_batch) =
            self.info_supported_function_pages(io, 0).await?;

        let total = usize::from(count) + usize::from(left);
        if total == 0 {
            debug!("OpenInput: device reports 0 function pages, skipping enumeration");
            return Ok(());
        }

        let mut pages: Vec<u8> = Vec::with_capacity(total);
        pages.extend_from_slice(&first_batch);
        let mut read = usize::from(count);
        let mut remaining = left;

        while remaining > 0 {
            let (c, l, batch) =
                self.info_supported_function_pages(io, read as u8).await?;

            /* Guard against inconsistent pagination responses. */
            if total != read + usize::from(c) + usize::from(l) {
                anyhow::bail!(
                    "OpenInput: invalid number of function pages left to read ({l})"
                );
            }
            debug!("OpenInput: read {c} pages, {l} left");
            pages.extend_from_slice(&batch);
            read += usize::from(c);
            remaining = l;
        }

        for &page in &pages {
            debug!("OpenInput: found function page {}", page_name(page));
            if let Err(e) = self.read_supported_functions(io, page).await {
                warn!("OpenInput: failed to read functions for page {}: {e}", page_name(page));
            }
        }

        Ok(())
    }
}

/* ------------------------------------------------------------------ */
/* DeviceDriver trait implementation                                    */
/* ------------------------------------------------------------------ */

#[async_trait]
impl DeviceDriver for OpenInputDriver {
    fn name(&self) -> &str {
        "OpenInput"
    }

    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()> {
        /* Initialise cached state so info_version() can write into it. */
        self.data = Some(OiData {
            fw_major: 0,
            fw_minor: 0,
            fw_patch: 0,
            num_profiles: 1,
            num_resolutions: 0,
            num_buttons: 0,
            num_leds: 0,
            supported: 0,
        });

        /* Step 1: query protocol version. */
        self.info_version(io).await
            .context("OpenInput probe: version query failed")?;

        /* Step 2: query firmware vendor, version string, and device name.
         * These are informational; a failure is non-fatal and only logged. */
        match self.info_fw_info(io, OI_FW_INFO_VENDOR).await {
            Ok(s) => info!("OpenInput: firmware vendor: {s}"),
            Err(e) => warn!("OpenInput: failed to read firmware vendor: {e}"),
        }
        match self.info_fw_info(io, OI_FW_INFO_VERSION).await {
            Ok(s) => info!("OpenInput: firmware version: {s}"),
            Err(e) => warn!("OpenInput: failed to read firmware version string: {e}"),
        }
        match self.info_fw_info(io, OI_FW_INFO_DEVICE_NAME).await {
            Ok(s) => info!("OpenInput: device: {s}"),
            Err(e) => warn!("OpenInput: failed to read device name: {e}"),
        }

        /* Step 3: enumerate supported function pages and their functions. */
        if let Err(e) = self.read_supported_function_pages(io).await {
            warn!("OpenInput: function page enumeration failed: {e}");
        }

        Ok(())
    }

    async fn load_profiles(&mut self, _io: &mut DeviceIo, info: &mut DeviceInfo) -> Result<()> {
        let _data = self.data.as_ref()
            .ok_or_else(|| anyhow::anyhow!("OpenInput: probe() must be called before load_profiles()"))?;

        /* The C driver only sets the report rate list and marks the single
         * profile active — openinput_read_profile().  We mirror that here. */
        for profile in &mut info.profiles {
            profile.report_rates = REPORT_RATES.to_vec();
            profile.is_active = true;
        }

        debug!("OpenInput: loaded {} profile(s)", info.profiles.len());
        Ok(())
    }

    async fn commit(&mut self, _io: &mut DeviceIo, _info: &DeviceInfo) -> Result<()> {
        /* The C reference driver has no commit function at all — no write
         * commands are implemented in the protocol yet.  This is intentionally
         * a no-op until write support is added. */
        debug!("OpenInput: commit called (no-op — no write commands implemented)");
        Ok(())
    }
}

/* ------------------------------------------------------------------ */
/* Helpers                                                              */
/* ------------------------------------------------------------------ */

/* Build a short OpenInput feature request. */
#[allow(dead_code)]
pub fn build_request(page: u8, function: u8) -> OiReport {
    OiReport {
        id: OI_REPORT_SHORT,
        function_page: page,
        function,
        data: [0u8; OI_REPORT_DATA_MAX_SIZE],
    }
}

/* Return a human-readable name for a function page. */
pub fn page_name(page: u8) -> &'static str {
    match page {
        0x00 => "INFO",
        0x01 => "SETTINGS",
        0x02 => "DPI",
        0x03 => "BUTTONS",
        0x04 => "LEDS",
        OI_PAGE_GIMMICKS => "GIMMICKS",
        OI_PAGE_DEBUG => "DEBUG",
        OI_PAGE_ERROR => "ERROR",
        _ => "UNKNOWN",
    }
}
