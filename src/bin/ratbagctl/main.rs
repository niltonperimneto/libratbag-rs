/* ratbagctl CLI: clap-driven client that talks to ratbagd over DBus to list devices, inspect and
 * modify profiles/resolutions/buttons/LEDs, and exercise dev-hook test devices. */
mod dbus_client;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use dbus_client::RatbagClient;

/// ratbagctl — configure gaming mice via the ratbagd DBus daemon.
#[derive(Parser)]
#[command(name = "ratbagctl", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List connected devices.
    List,

    /// Show detailed information about a device.
    Info {
        /// Device index (0-based, from `ratbagctl list`) or sysname.
        device: String,
    },

    /// Commit pending changes to hardware.
    Commit {
        /// Device index or sysname.
        device: String,
    },

    /// Profile commands.
    #[command(subcommand)]
    Profile(ProfileCmd),

    /// Resolution (DPI) commands.
    #[command(subcommand)]
    Resolution(ResolutionCmd),

    /// Button mapping commands.
    #[command(subcommand)]
    Button(ButtonCmd),

    /// LED commands.
    #[command(subcommand)]
    Led(LedCmd),

    /// Dev-hooks test commands (requires daemon built with dev-hooks).
    #[command(subcommand)]
    Test(TestCmd),
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// List profiles for a device.
    List {
        /// Device index or sysname.
        device: String,
    },
    /// Show profile details.
    Info {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
    },
    /// Set the active profile.
    Active {
        /// Device index or sysname.
        device: String,
        /// Profile index to activate.
        profile: u32,
    },
    /// Get or set the profile name.
    Name {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// New name (omit to read current).
        name: Option<String>,
    },
    /// Enable a profile.
    Enable {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
    },
    /// Disable a profile.
    Disable {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
    },
    /// Set the report rate for a profile.
    Rate {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// Report rate in Hz.
        rate: u32,
    },
    /// Get or set angle snapping (on/off).
    #[command(name = "angle-snapping")]
    AngleSnapping {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// New value: "on" or "off" (omit to read current).
        value: Option<String>,
    },
    /// Get or set debounce time in ms.
    Debounce {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// New debounce time in ms (omit to read current + supported values).
        ms: Option<i32>,
    },
}

#[derive(Subcommand)]
enum ResolutionCmd {
    /// List resolutions for a profile.
    List {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
    },
    /// Get or set DPI for a resolution.
    Dpi {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// Resolution index.
        resolution: u32,
        /// New DPI value (omit to read current).
        dpi: Option<u32>,
    },
    /// Set the active resolution.
    Active {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// Resolution index to activate.
        resolution: u32,
    },
    /// Set the default resolution.
    Default {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// Resolution index to make default.
        resolution: u32,
    },
    /// Enable a resolution slot.
    Enable {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// Resolution index.
        resolution: u32,
    },
    /// Disable a resolution slot.
    Disable {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// Resolution index.
        resolution: u32,
    },
}

#[derive(Subcommand)]
enum ButtonCmd {
    /// List buttons for a profile.
    List {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
    },
    /// Get current button mapping.
    Get {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// Button index.
        button: u32,
    },
    /// Set button to a simple button mapping (action type 1).
    #[command(name = "set-button")]
    SetButton {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// Button index.
        button: u32,
        /// Logical button number to map to.
        value: u32,
    },
    /// Set button to a special action (action type 2).
    #[command(name = "set-special")]
    SetSpecial {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// Button index.
        button: u32,
        /// Special action code.
        value: u32,
    },
    /// Set button to a key mapping (action type 3).
    #[command(name = "set-key")]
    SetKey {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// Button index.
        button: u32,
        /// Linux keycode value.
        keycode: u32,
    },
    /// Set button to a macro (action type 4).
    ///
    /// Events are specified as KEYCODE:DIRECTION pairs separated by spaces,
    /// where DIRECTION is 1 for press and 0 for release.
    /// Example: "30:1 30:0" (press and release KEY_A).
    #[command(name = "set-macro")]
    SetMacro {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// Button index.
        button: u32,
        /// Macro events as "KEYCODE:DIR KEYCODE:DIR …".
        events: Vec<String>,
    },
    /// Disable a button (action type 0).
    Disable {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// Button index.
        button: u32,
    },
}

#[derive(Subcommand)]
enum LedCmd {
    /// List LEDs for a profile.
    List {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
    },
    /// Get LED info.
    Get {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// LED index.
        led: u32,
    },
    /// Set LED mode.
    Mode {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// LED index.
        led: u32,
        /// Mode: off, solid, cycle, wave, starlight, breathing, tricolor.
        mode: String,
    },
    /// Set LED primary color (hex RGB, e.g. ff0000).
    Color {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// LED index.
        led: u32,
        /// Hex RGB color (e.g. ff0000 for red).
        color: String,
    },
    /// Set LED secondary color (for multi-color effects like Starlight).
    #[command(name = "secondary-color")]
    SecondaryColor {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// LED index.
        led: u32,
        /// Hex RGB color.
        color: String,
    },
    /// Set LED tertiary color (for 3-zone effects like TriColor).
    #[command(name = "tertiary-color")]
    TertiaryColor {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// LED index.
        led: u32,
        /// Hex RGB color.
        color: String,
    },
    /// Set LED brightness (0-255).
    Brightness {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// LED index.
        led: u32,
        /// Brightness value 0-255.
        value: u32,
    },
    /// Set LED effect duration in ms (0-10000).
    Duration {
        /// Device index or sysname.
        device: String,
        /// Profile index.
        profile: u32,
        /// LED index.
        led: u32,
        /// Duration in milliseconds (0-10000).
        ms: u32,
    },
}

#[derive(Subcommand)]
enum TestCmd {
    /// Load a synthetic test device from a JSON file.
    #[command(name = "load-device")]
    LoadDevice {
        /// Path to a JSON file describing the test device.
        json_file: String,
    },
    /// Remove all test devices.
    Reset,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = RatbagClient::connect()
        .await
        .context("Failed to connect to ratbagd on org.freedesktop.ratbag1")?;

    match cli.command {
        Commands::List => cmd_list(&client).await,
        Commands::Info { device } => cmd_info(&client, &device).await,
        Commands::Commit { device } => cmd_commit(&client, &device).await,
        Commands::Profile(sub) => match sub {
            ProfileCmd::List { device } => cmd_profile_list(&client, &device).await,
            ProfileCmd::Info { device, profile } => {
                cmd_profile_info(&client, &device, profile).await
            }
            ProfileCmd::Active { device, profile } => {
                cmd_profile_active(&client, &device, profile).await
            }
            ProfileCmd::Name {
                device,
                profile,
                name,
            } => cmd_profile_name(&client, &device, profile, name).await,
            ProfileCmd::Enable { device, profile } => {
                cmd_profile_enable_disable(&client, &device, profile, false).await
            }
            ProfileCmd::Disable { device, profile } => {
                cmd_profile_enable_disable(&client, &device, profile, true).await
            }
            ProfileCmd::Rate {
                device,
                profile,
                rate,
            } => cmd_profile_rate(&client, &device, profile, rate).await,
            ProfileCmd::AngleSnapping {
                device,
                profile,
                value,
            } => cmd_profile_angle_snapping(&client, &device, profile, value).await,
            ProfileCmd::Debounce {
                device,
                profile,
                ms,
            } => cmd_profile_debounce(&client, &device, profile, ms).await,
        },
        Commands::Resolution(sub) => match sub {
            ResolutionCmd::List { device, profile } => {
                cmd_resolution_list(&client, &device, profile).await
            }
            ResolutionCmd::Dpi {
                device,
                profile,
                resolution,
                dpi,
            } => cmd_resolution_dpi(&client, &device, profile, resolution, dpi).await,
            ResolutionCmd::Active {
                device,
                profile,
                resolution,
            } => cmd_resolution_active(&client, &device, profile, resolution).await,
            ResolutionCmd::Default {
                device,
                profile,
                resolution,
            } => cmd_resolution_default(&client, &device, profile, resolution).await,
            ResolutionCmd::Enable {
                device,
                profile,
                resolution,
            } => cmd_resolution_enable_disable(&client, &device, profile, resolution, false).await,
            ResolutionCmd::Disable {
                device,
                profile,
                resolution,
            } => cmd_resolution_enable_disable(&client, &device, profile, resolution, true).await,
        },
        Commands::Button(sub) => match sub {
            ButtonCmd::List { device, profile } => {
                cmd_button_list(&client, &device, profile).await
            }
            ButtonCmd::Get {
                device,
                profile,
                button,
            } => cmd_button_get(&client, &device, profile, button).await,
            ButtonCmd::SetButton {
                device,
                profile,
                button,
                value,
            } => cmd_button_set(&client, &device, profile, button, 1, value).await,
            ButtonCmd::SetSpecial {
                device,
                profile,
                button,
                value,
            } => cmd_button_set(&client, &device, profile, button, 2, value).await,
            ButtonCmd::SetKey {
                device,
                profile,
                button,
                keycode,
            } => cmd_button_set(&client, &device, profile, button, 3, keycode).await,
            ButtonCmd::SetMacro {
                device,
                profile,
                button,
                events,
            } => cmd_button_set_macro(&client, &device, profile, button, &events).await,
            ButtonCmd::Disable {
                device,
                profile,
                button,
            } => cmd_button_set(&client, &device, profile, button, 0, 0).await,
        },
        Commands::Led(sub) => match sub {
            LedCmd::List { device, profile } => cmd_led_list(&client, &device, profile).await,
            LedCmd::Get {
                device,
                profile,
                led,
            } => cmd_led_get(&client, &device, profile, led).await,
            LedCmd::Mode {
                device,
                profile,
                led,
                mode,
            } => cmd_led_mode(&client, &device, profile, led, &mode).await,
            LedCmd::Color {
                device,
                profile,
                led,
                color,
            } => cmd_led_color(&client, &device, profile, led, &color, "Color").await,
            LedCmd::SecondaryColor {
                device,
                profile,
                led,
                color,
            } => cmd_led_color(&client, &device, profile, led, &color, "SecondaryColor").await,
            LedCmd::TertiaryColor {
                device,
                profile,
                led,
                color,
            } => cmd_led_color(&client, &device, profile, led, &color, "TertiaryColor").await,
            LedCmd::Brightness {
                device,
                profile,
                led,
                value,
            } => cmd_led_brightness(&client, &device, profile, led, value).await,
            LedCmd::Duration {
                device,
                profile,
                led,
                ms,
            } => cmd_led_duration(&client, &device, profile, led, ms).await,
        },
        Commands::Test(sub) => match sub {
            TestCmd::LoadDevice { json_file } => cmd_test_load_device(&client, &json_file).await,
            TestCmd::Reset => cmd_test_reset(&client).await,
        },
    }
}

// ---------------------------------------------------------------------------
// Helpers: resolve paths and auto-commit
// ---------------------------------------------------------------------------

/// Derive the device object path from a sub-object path (e.g. .../p0/r1 -> .../device).
fn device_path_from_child(child_path: &str) -> &str {
    // Profile paths look like /org/freedesktop/ratbag1/device/<sysname>/p0
    // Resolution paths:       /org/freedesktop/ratbag1/device/<sysname>/p0/r1
    // We need to strip everything from /p onward.
    if let Some(idx) = child_path.find("/p") {
        &child_path[..idx]
    } else {
        child_path
    }
}

/// Commit changes to hardware after a write operation.
async fn auto_commit(client: &RatbagClient, any_path: &str) -> Result<()> {
    let dev_path = device_path_from_child(any_path);
    let rc = client.commit_device(dev_path).await?;
    if rc != 0 {
        anyhow::bail!("Commit returned error code {}", rc);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

async fn cmd_list(client: &RatbagClient) -> Result<()> {
    let api = client.get_api_version().await.unwrap_or(-1);
    let devices = client.list_devices().await?;
    if devices.is_empty() {
        println!("No devices found. (API version {})", api);
        return Ok(());
    }
    println!("API version: {}", api);
    for (i, path) in devices.iter().enumerate() {
        let name = client.get_device_name(path).await.unwrap_or_default();
        let model = client.get_device_model(path).await.unwrap_or_default();
        println!("{}: {} ({})", i, name, model);
    }
    Ok(())
}

async fn cmd_info(client: &RatbagClient, device: &str) -> Result<()> {
    let path = client.resolve_device(device).await?;
    let name = client.get_device_name(&path).await?;
    let model = client.get_device_model(&path).await?;
    let fw = client.get_device_firmware(&path).await?;
    let profiles = client.get_device_profiles(&path).await?;
    println!("Device:    {}", name);
    println!("Model:     {}", model);
    if !fw.is_empty() {
        println!("Firmware:  {}", fw);
    }
    println!("Profiles:  {}", profiles.len());
    for profile_path in &profiles {
        let idx = client.get_profile_index(profile_path).await?;
        let active = client.get_profile_is_active(profile_path).await?;
        let rate = client.get_profile_report_rate(profile_path).await?;
        let pname = client.get_profile_name(profile_path).await.unwrap_or_default();
        let name_display = if pname.is_empty() {
            String::new()
        } else {
            format!(" \"{}\"", pname)
        };
        println!(
            "  Profile {}{}: rate={}Hz{}",
            idx,
            name_display,
            rate,
            if active { " [active]" } else { "" }
        );
    }
    Ok(())
}

async fn cmd_commit(client: &RatbagClient, device: &str) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let rc = client.commit_device(&dev_path).await?;
    if rc != 0 {
        anyhow::bail!("Commit returned error code {}", rc);
    }
    println!("Changes committed to hardware.");
    Ok(())
}

async fn cmd_profile_list(client: &RatbagClient, device: &str) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let profiles = client.get_device_profiles(&dev_path).await?;
    for profile_path in &profiles {
        let idx = client.get_profile_index(profile_path).await?;
        let active = client.get_profile_is_active(profile_path).await?;
        let enabled = !client.get_profile_disabled(profile_path).await?;
        let rate = client.get_profile_report_rate(profile_path).await?;
        let pname = client.get_profile_name(profile_path).await.unwrap_or_default();
        let dirty = client.get_profile_is_dirty(profile_path).await.unwrap_or(false);
        let name_display = if pname.is_empty() {
            String::new()
        } else {
            format!(" \"{}\"", pname)
        };
        println!(
            "Profile {}{}: rate={}Hz enabled={} active={}{}",
            idx, name_display, rate, enabled, active,
            if dirty { " [dirty]" } else { "" }
        );
    }
    Ok(())
}

async fn cmd_profile_info(client: &RatbagClient, device: &str, profile: u32) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let profile_path = format!("{}/p{}", dev_path, profile);
    let idx = client.get_profile_index(&profile_path).await?;
    let active = client.get_profile_is_active(&profile_path).await?;
    let disabled = client.get_profile_disabled(&profile_path).await?;
    let dirty = client.get_profile_is_dirty(&profile_path).await.unwrap_or(false);
    let pname = client.get_profile_name(&profile_path).await.unwrap_or_default();
    let rate = client.get_profile_report_rate(&profile_path).await?;
    let rates = client.get_profile_report_rates(&profile_path).await?;
    let angle = client.get_profile_angle_snapping(&profile_path).await?;
    let debounce = client.get_profile_debounce(&profile_path).await?;
    let debounces = client.get_profile_debounces(&profile_path).await.unwrap_or_default();

    println!("Profile {}:", idx);
    if !pname.is_empty() {
        println!("  Name:           {}", pname);
    }
    println!("  Active:         {}", active);
    println!("  Enabled:        {}", !disabled);
    println!("  Dirty:          {}", dirty);
    println!("  Report rate:    {} Hz", rate);
    println!("  Supported rates: {:?}", rates);
    if angle >= 0 {
        println!(
            "  Angle snapping: {}",
            if angle == 1 { "on" } else { "off" }
        );
    }
    if debounce >= 0 {
        println!("  Debounce:       {} ms", debounce);
    }
    if !debounces.is_empty() {
        println!("  Supported debounces: {:?}", debounces);
    }

    let resolutions = client.get_profile_resolutions(&profile_path).await?;
    for res_path in &resolutions {
        let ri = client.get_resolution_index(res_path).await?;
        let dpi = client.get_resolution_dpi(res_path).await?;
        let res_active = client.get_resolution_is_active(res_path).await?;
        let dpi_list = client.get_resolution_dpi_list(res_path).await.unwrap_or_default();
        let dpi_info = if dpi_list.is_empty() {
            String::new()
        } else {
            format!(" (supported: {:?})", dpi_list)
        };
        println!(
            "  Resolution {}: {}{}{}",
            ri,
            dpi,
            if res_active { " [active]" } else { "" },
            dpi_info,
        );
    }

    let buttons = client.get_profile_buttons(&profile_path).await?;
    for btn_path in &buttons {
        let bi = client.get_button_index(btn_path).await?;
        let (action_type, mapping_val) = client.get_button_mapping(btn_path).await?;
        println!(
            "  Button {}: type={} value={}",
            bi,
            action_type_name(action_type),
            mapping_val
        );
    }

    let leds = client.get_profile_leds(&profile_path).await?;
    for led_path in &leds {
        let li = client.get_led_index(led_path).await?;
        let mode = client.get_led_mode(led_path).await?;
        let (r, g, b) = client.get_led_color(led_path).await?;
        let bright = client.get_led_brightness(led_path).await?;
        let duration = client.get_led_effect_duration(led_path).await?;
        println!(
            "  LED {}: mode={} color=#{:02x}{:02x}{:02x} brightness={} duration={}ms",
            li,
            led_mode_name(mode),
            r,
            g,
            b,
            bright,
            duration,
        );
    }
    Ok(())
}

async fn cmd_profile_active(client: &RatbagClient, device: &str, profile: u32) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let profile_path = format!("{}/p{}", dev_path, profile);
    client.call_profile_set_active(&profile_path).await?;
    auto_commit(client, &profile_path).await?;
    println!("Profile {} set as active.", profile);
    Ok(())
}

async fn cmd_profile_name(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    name: Option<String>,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let profile_path = format!("{}/p{}", dev_path, profile);
    match name {
        Some(n) => {
            client.set_profile_name(&profile_path, &n).await?;
            auto_commit(client, &profile_path).await?;
            println!("Profile {} name set to \"{}\".", profile, n);
        }
        None => {
            let n = client.get_profile_name(&profile_path).await?;
            if n.is_empty() {
                println!("Profile {} has no name set.", profile);
            } else {
                println!("{}", n);
            }
        }
    }
    Ok(())
}

async fn cmd_profile_enable_disable(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    disable: bool,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let profile_path = format!("{}/p{}", dev_path, profile);
    client.set_profile_disabled(&profile_path, disable).await?;
    auto_commit(client, &profile_path).await?;
    println!(
        "Profile {} {}.",
        profile,
        if disable { "disabled" } else { "enabled" }
    );
    Ok(())
}

async fn cmd_profile_rate(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    rate: u32,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let profile_path = format!("{}/p{}", dev_path, profile);
    client.set_profile_report_rate(&profile_path, rate).await?;
    auto_commit(client, &profile_path).await?;
    println!("Profile {} report rate set to {} Hz.", profile, rate);
    Ok(())
}

async fn cmd_profile_angle_snapping(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    value: Option<String>,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let profile_path = format!("{}/p{}", dev_path, profile);
    match value {
        Some(v) => {
            let val = match v.to_lowercase().as_str() {
                "on" | "1" | "true" | "yes" => 1,
                "off" | "0" | "false" | "no" => 0,
                _ => anyhow::bail!("Invalid angle-snapping value '{}'. Use: on, off", v),
            };
            client
                .set_profile_angle_snapping(&profile_path, val)
                .await?;
            auto_commit(client, &profile_path).await?;
            println!(
                "Profile {} angle snapping set to {}.",
                profile,
                if val == 1 { "on" } else { "off" }
            );
        }
        None => {
            let angle = client.get_profile_angle_snapping(&profile_path).await?;
            if angle < 0 {
                println!("Angle snapping is not supported on this device.");
            } else {
                println!("{}", if angle == 1 { "on" } else { "off" });
            }
        }
    }
    Ok(())
}

async fn cmd_profile_debounce(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    ms: Option<i32>,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let profile_path = format!("{}/p{}", dev_path, profile);
    match ms {
        Some(val) => {
            client.set_profile_debounce(&profile_path, val).await?;
            auto_commit(client, &profile_path).await?;
            println!("Profile {} debounce set to {} ms.", profile, val);
        }
        None => {
            let debounce = client.get_profile_debounce(&profile_path).await?;
            let debounces = client
                .get_profile_debounces(&profile_path)
                .await
                .unwrap_or_default();
            if debounce < 0 {
                println!("Debounce is not supported on this device.");
            } else {
                println!("Current: {} ms", debounce);
                if !debounces.is_empty() {
                    println!("Supported: {:?}", debounces);
                }
            }
        }
    }
    Ok(())
}

async fn cmd_resolution_list(client: &RatbagClient, device: &str, profile: u32) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let profile_path = format!("{}/p{}", dev_path, profile);
    let resolutions = client.get_profile_resolutions(&profile_path).await?;
    for res_path in &resolutions {
        let idx = client.get_resolution_index(res_path).await?;
        let dpi = client.get_resolution_dpi(res_path).await?;
        let active = client.get_resolution_is_active(res_path).await?;
        let default = client.get_resolution_is_default(res_path).await?;
        let disabled = client.get_resolution_is_disabled(res_path).await?;
        let caps = client
            .get_resolution_capabilities(res_path)
            .await
            .unwrap_or_default();
        let dpi_list = client
            .get_resolution_dpi_list(res_path)
            .await
            .unwrap_or_default();
        let mut flags = Vec::new();
        if active {
            flags.push("[active]");
        }
        if default {
            flags.push("[default]");
        }
        if disabled {
            flags.push("[disabled]");
        }
        let flags_str = if flags.is_empty() {
            String::new()
        } else {
            format!(" {}", flags.join(" "))
        };
        let dpi_info = if dpi_list.is_empty() {
            String::new()
        } else {
            format!(" (supported: {:?})", dpi_list)
        };
        let caps_info = if caps.is_empty() {
            String::new()
        } else {
            format!(" caps={:?}", caps)
        };
        println!("Resolution {}: {}{}{}{}", idx, dpi, flags_str, dpi_info, caps_info);
    }
    Ok(())
}

async fn cmd_resolution_dpi(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    resolution: u32,
    dpi: Option<u32>,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let res_path = format!("{}/p{}/r{}", dev_path, profile, resolution);
    match dpi {
        Some(val) => {
            client.set_resolution_dpi(&res_path, val).await?;
            auto_commit(client, &res_path).await?;
            println!("Resolution {} DPI set to {}.", resolution, val);
        }
        None => {
            let current = client.get_resolution_dpi(&res_path).await?;
            let dpi_list = client
                .get_resolution_dpi_list(&res_path)
                .await
                .unwrap_or_default();
            println!("{}", current);
            if !dpi_list.is_empty() {
                println!("Supported: {:?}", dpi_list);
            }
        }
    }
    Ok(())
}

async fn cmd_resolution_active(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    resolution: u32,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let res_path = format!("{}/p{}/r{}", dev_path, profile, resolution);
    client.call_resolution_set_active(&res_path).await?;
    auto_commit(client, &res_path).await?;
    println!("Resolution {} set as active.", resolution);
    Ok(())
}

async fn cmd_resolution_default(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    resolution: u32,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let res_path = format!("{}/p{}/r{}", dev_path, profile, resolution);
    client.call_resolution_set_default(&res_path).await?;
    auto_commit(client, &res_path).await?;
    println!("Resolution {} set as default.", resolution);
    Ok(())
}

async fn cmd_resolution_enable_disable(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    resolution: u32,
    disable: bool,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let res_path = format!("{}/p{}/r{}", dev_path, profile, resolution);
    client
        .set_resolution_is_disabled(&res_path, disable)
        .await?;
    auto_commit(client, &res_path).await?;
    println!(
        "Resolution {} {}.",
        resolution,
        if disable { "disabled" } else { "enabled" }
    );
    Ok(())
}

async fn cmd_button_list(client: &RatbagClient, device: &str, profile: u32) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let profile_path = format!("{}/p{}", dev_path, profile);
    let buttons = client.get_profile_buttons(&profile_path).await?;
    for btn_path in &buttons {
        let idx = client.get_button_index(btn_path).await?;
        let (action_type, mapping_val) = client.get_button_mapping(btn_path).await?;
        println!(
            "Button {}: type={} value={}",
            idx,
            action_type_name(action_type),
            mapping_val
        );
    }
    Ok(())
}

async fn cmd_button_get(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    button: u32,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let btn_path = format!("{}/p{}/b{}", dev_path, profile, button);
    let (action_type, mapping_val) = client.get_button_mapping(&btn_path).await?;
    let action_types = client.get_button_action_types(&btn_path).await?;
    println!("Button {}:", button);
    println!(
        "  Action type: {} ({})",
        action_type_name(action_type),
        action_type
    );
    println!("  Value:       {}", mapping_val);
    println!(
        "  Supported:   {:?}",
        action_types
            .iter()
            .map(|t| action_type_name(*t))
            .collect::<Vec<_>>()
    );
    Ok(())
}

async fn cmd_button_set(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    button: u32,
    action_type: u32,
    value: u32,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let btn_path = format!("{}/p{}/b{}", dev_path, profile, button);
    client
        .set_button_mapping(&btn_path, action_type, value)
        .await?;
    auto_commit(client, &btn_path).await?;
    println!(
        "Button {} set to {}={}.",
        button,
        action_type_name(action_type),
        value
    );
    Ok(())
}

async fn cmd_button_set_macro(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    button: u32,
    events: &[String],
) -> Result<()> {
    let parsed = parse_macro_events(events)?;
    let dev_path = client.resolve_device(device).await?;
    let btn_path = format!("{}/p{}/b{}", dev_path, profile, button);
    client
        .set_button_macro_mapping(&btn_path, &parsed)
        .await?;
    auto_commit(client, &btn_path).await?;
    println!("Button {} set to macro ({} events).", button, parsed.len());
    Ok(())
}

async fn cmd_led_list(client: &RatbagClient, device: &str, profile: u32) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let profile_path = format!("{}/p{}", dev_path, profile);
    let leds = client.get_profile_leds(&profile_path).await?;
    for led_path in &leds {
        let idx = client.get_led_index(led_path).await?;
        let mode = client.get_led_mode(led_path).await?;
        let (r, g, b) = client.get_led_color(led_path).await?;
        let bright = client.get_led_brightness(led_path).await?;
        println!(
            "LED {}: mode={} color=#{:02x}{:02x}{:02x} brightness={}",
            idx,
            led_mode_name(mode),
            r,
            g,
            b,
            bright
        );
    }
    Ok(())
}

async fn cmd_led_get(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    led: u32,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let led_path = format!("{}/p{}/l{}", dev_path, profile, led);
    let mode = client.get_led_mode(&led_path).await?;
    let modes = client.get_led_modes(&led_path).await?;
    let (r, g, b) = client.get_led_color(&led_path).await?;
    let (sr, sg, sb) = client.get_led_secondary_color(&led_path).await?;
    let (tr, tg, tb) = client.get_led_tertiary_color(&led_path).await?;
    let bright = client.get_led_brightness(&led_path).await?;
    let duration = client.get_led_effect_duration(&led_path).await?;
    let depth = client.get_led_color_depth(&led_path).await.unwrap_or(0);
    println!("LED {}:", led);
    println!("  Mode:            {}", led_mode_name(mode));
    println!("  Color:           #{:02x}{:02x}{:02x}", r, g, b);
    println!("  Secondary color: #{:02x}{:02x}{:02x}", sr, sg, sb);
    println!("  Tertiary color:  #{:02x}{:02x}{:02x}", tr, tg, tb);
    println!("  Brightness:      {}", bright);
    println!("  Duration:        {} ms", duration);
    println!("  Color depth:     {}", color_depth_name(depth));
    println!(
        "  Supported modes: {:?}",
        modes
            .iter()
            .map(|m| led_mode_name(*m))
            .collect::<Vec<_>>()
    );
    Ok(())
}

async fn cmd_led_mode(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    led: u32,
    mode: &str,
) -> Result<()> {
    let mode_val = parse_led_mode(mode)?;
    let dev_path = client.resolve_device(device).await?;
    let led_path = format!("{}/p{}/l{}", dev_path, profile, led);
    client.set_led_mode(&led_path, mode_val).await?;
    auto_commit(client, &led_path).await?;
    println!("LED {} mode set to {}.", led, mode);
    Ok(())
}

async fn cmd_led_color(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    led: u32,
    color: &str,
    which: &str,
) -> Result<()> {
    let (r, g, b) = parse_hex_color(color)?;
    let dev_path = client.resolve_device(device).await?;
    let led_path = format!("{}/p{}/l{}", dev_path, profile, led);
    match which {
        "SecondaryColor" => client.set_led_secondary_color(&led_path, r, g, b).await?,
        "TertiaryColor" => client.set_led_tertiary_color(&led_path, r, g, b).await?,
        _ => client.set_led_color(&led_path, r, g, b).await?,
    }
    auto_commit(client, &led_path).await?;
    let label = match which {
        "SecondaryColor" => "secondary color",
        "TertiaryColor" => "tertiary color",
        _ => "color",
    };
    println!("LED {} {} set to #{:02x}{:02x}{:02x}.", led, label, r, g, b);
    Ok(())
}

async fn cmd_led_brightness(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    led: u32,
    value: u32,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let led_path = format!("{}/p{}/l{}", dev_path, profile, led);
    client.set_led_brightness(&led_path, value).await?;
    auto_commit(client, &led_path).await?;
    println!("LED {} brightness set to {}.", led, value);
    Ok(())
}

async fn cmd_led_duration(
    client: &RatbagClient,
    device: &str,
    profile: u32,
    led: u32,
    ms: u32,
) -> Result<()> {
    let dev_path = client.resolve_device(device).await?;
    let led_path = format!("{}/p{}/l{}", dev_path, profile, led);
    client.set_led_effect_duration(&led_path, ms).await?;
    auto_commit(client, &led_path).await?;
    println!("LED {} effect duration set to {} ms.", led, ms);
    Ok(())
}

async fn cmd_test_load_device(client: &RatbagClient, json_file: &str) -> Result<()> {
    let json = std::fs::read_to_string(json_file)
        .with_context(|| format!("Cannot read file '{}'", json_file))?;
    let path = client.load_test_device(&json).await?;
    println!("Test device loaded at {}.", path);
    Ok(())
}

async fn cmd_test_reset(client: &RatbagClient) -> Result<()> {
    client.reset_test_device().await?;
    println!("All test devices removed.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn action_type_name(t: u32) -> &'static str {
    match t {
        0 => "none",
        1 => "button",
        2 => "special",
        3 => "key",
        4 => "macro",
        _ => "unknown",
    }
}

fn led_mode_name(m: u32) -> &'static str {
    match m {
        0 => "off",
        1 => "solid",
        3 => "cycle",
        4 => "wave",
        5 => "starlight",
        10 => "breathing",
        32 => "tricolor",
        _ => "unknown",
    }
}

fn color_depth_name(d: u32) -> &'static str {
    match d {
        0 => "monochrome",
        1 => "rgb",
        _ => "unknown",
    }
}

fn parse_led_mode(s: &str) -> Result<u32> {
    match s.to_lowercase().as_str() {
        "off" => Ok(0),
        "solid" => Ok(1),
        "cycle" => Ok(3),
        "wave" | "colorwave" | "color-wave" => Ok(4),
        "starlight" => Ok(5),
        "breathing" | "breathe" => Ok(10),
        "tricolor" | "tri-color" => Ok(32),
        _ => anyhow::bail!(
            "Unknown LED mode '{}'. Use: off, solid, cycle, wave, starlight, breathing, tricolor",
            s
        ),
    }
}

fn parse_hex_color(s: &str) -> Result<(u32, u32, u32)> {
    let s = s.strip_prefix('#').unwrap_or(s);
    anyhow::ensure!(
        s.len() == 6,
        "Color must be a 6-digit hex string (e.g. ff0000)"
    );
    let r = u32::from_str_radix(&s[0..2], 16).context("Invalid red component")?;
    let g = u32::from_str_radix(&s[2..4], 16).context("Invalid green component")?;
    let b = u32::from_str_radix(&s[4..6], 16).context("Invalid blue component")?;
    Ok((r, g, b))
}

/// Parse macro events from CLI arguments.
///
/// Each argument is "KEYCODE:DIRECTION" where DIRECTION is 1 (press) or 0 (release).
/// Example: `["30:1", "30:0"]` = press KEY_A then release KEY_A.
fn parse_macro_events(events: &[String]) -> Result<Vec<(u32, u32)>> {
    let mut parsed = Vec::with_capacity(events.len());
    for ev in events {
        let parts: Vec<&str> = ev.split(':').collect();
        anyhow::ensure!(
            parts.len() == 2,
            "Invalid macro event '{}'. Expected KEYCODE:DIRECTION (e.g. 30:1)",
            ev
        );
        let keycode: u32 = parts[0]
            .parse()
            .with_context(|| format!("Invalid keycode in '{}'", ev))?;
        let direction: u32 = parts[1]
            .parse()
            .with_context(|| format!("Invalid direction in '{}'", ev))?;
        anyhow::ensure!(
            direction <= 1,
            "Direction must be 0 (release) or 1 (press), got {} in '{}'",
            direction,
            ev
        );
        parsed.push((keycode, direction));
    }
    Ok(parsed)
}
