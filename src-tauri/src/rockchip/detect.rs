// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (c) 2026 Daniele Briguglio, superkali@armbian.com

//! Rockchip USB device detection: lists boards in Maskrom or Loader mode (VID 0x2207) via nusb.
//! Enumeration only, the panel polls it every few seconds; nothing here opens or claims a device.

use nusb::{DeviceInfo, MaybeFuture};

use super::{
    socs, RockchipDevice, RockchipMode, ROCKCHIP_VID, ROCKUSB_PATH_PREFIX,
    TAG_ROCKUSB_DEVICE_NOT_FOUND, TAG_ROCKUSB_MULTIPLE_DEVICES,
};
use crate::devices::TAG_INVALID_PATH;
use crate::log_debug;

/// rkdeveloptool's rockusb filter: Rockchip VID with a PID above 0x00ff (0x0000/0x0010 are mass storage).
fn is_rockusb(vendor_id: u16, product_id: u16) -> bool {
    vendor_id == ROCKCHIP_VID && (product_id >> 8) > 0
}

/// Device path for the board on `port_chain` of `bus_id`; stable while it re-enumerates on that port.
pub fn rockusb_device_path(bus_id: &str, port_chain: &[u8]) -> String {
    let ports: Vec<String> = port_chain.iter().map(u8::to_string).collect();
    format!("{ROCKUSB_PATH_PREFIX}{bus_id}/{}", ports.join("."))
}

fn describe(info: &DeviceInfo) -> RockchipDevice {
    let mode = RockchipMode::from_bcd_usb(info.usb_version());
    let pid = info.product_id();
    // Only the boot ROM PID is chip specific; U-Boot's rockusb gadget reuses 0x350a/0x330a across SoCs.
    let soc = match mode {
        RockchipMode::Maskrom => socs::find_by_maskrom_pid(pid),
        RockchipMode::Loader => None,
    };
    RockchipDevice {
        path: rockusb_device_path(info.bus_id(), info.port_chain()),
        mode,
        product_id: pid,
        soc: soc.map(|s| s.name.to_string()),
        maskrom_supported: soc.is_some(),
        bus_id: info.bus_id().to_string(),
        port_chain: info
            .port_chain()
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join("."),
    }
}

pub(crate) fn rockusb_device_infos() -> Result<Vec<DeviceInfo>, String> {
    Ok(nusb::list_devices()
        .wait()
        .map_err(|e| format!("Failed to enumerate USB devices: {}", e))?
        .filter(|dev| is_rockusb(dev.vendor_id(), dev.product_id()))
        .collect())
}

/// Detect connected Rockchip boards in Maskrom or Loader mode
pub fn get_rockusb_devices() -> Result<Vec<RockchipDevice>, String> {
    let devices: Vec<RockchipDevice> = rockusb_device_infos()?.iter().map(describe).collect();
    log_debug!(
        "rockchip::detect",
        "Found {} Rockchip USB device(s)",
        devices.len()
    );
    Ok(devices)
}

/// The one candidate at `path`; refused when none or several share it.
pub(crate) fn select_by_path<'a, T>(
    path: &str,
    candidates: &'a [T],
    path_of: impl Fn(&T) -> String,
) -> Result<&'a T, String> {
    if !path.starts_with(ROCKUSB_PATH_PREFIX) {
        return Err(format!(
            "{TAG_INVALID_PATH} {path:?} is not a Rockchip USB device path"
        ));
    }
    let matches: Vec<&T> = candidates.iter().filter(|c| path_of(c) == path).collect();
    match matches.as_slice() {
        [] => Err(format!(
            "{TAG_ROCKUSB_DEVICE_NOT_FOUND} {path:?} is not a connected Rockchip USB device"
        )),
        [one] => Ok(*one),
        many => Err(format!(
            "{TAG_ROCKUSB_MULTIPLE_DEVICES} {path:?} matches {} Rockchip USB devices",
            many.len()
        )),
    }
}

/// Scan now and return the device at `path` with its current mode; call right before opening it.
pub(crate) fn resolve(path: &str) -> Result<(DeviceInfo, RockchipDevice), String> {
    let infos = rockusb_device_infos()?;
    let info = select_by_path(path, &infos, |i| {
        rockusb_device_path(i.bus_id(), i.port_chain())
    })?;
    Ok((info.clone(), describe(info)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rockusb_filter_matches_rkdeveloptool() {
        assert!(is_rockusb(0x2207, 0x350b));
        assert!(is_rockusb(0x2207, 0x0100));
        assert!(!is_rockusb(0x2207, 0x0010));
        assert!(!is_rockusb(0x2207, 0x0000));
        assert!(!is_rockusb(0x05c6, 0x9008));
    }

    #[test]
    fn bcd_usb_bit_zero_selects_the_mode() {
        assert_eq!(RockchipMode::from_bcd_usb(0x0200), RockchipMode::Maskrom);
        assert_eq!(RockchipMode::from_bcd_usb(0x0201), RockchipMode::Loader);
        assert_eq!(RockchipMode::from_bcd_usb(0x0210), RockchipMode::Maskrom);
        assert_eq!(RockchipMode::from_bcd_usb(0x0211), RockchipMode::Loader);
    }

    #[test]
    fn device_path_follows_the_port_not_the_address() {
        assert_eq!(rockusb_device_path("1", &[2, 3]), "rockusb://1/2.3");
        assert_eq!(rockusb_device_path("1", &[]), "rockusb://1/");
        let windows_bus = "PCIROOT(0)#PCI(1400)#USBROOT(0)";
        assert_eq!(
            rockusb_device_path(windows_bus, &[4]),
            format!("rockusb://{windows_bus}/4")
        );
    }

    #[test]
    fn selection_needs_exactly_one_match() {
        let c = vec!["rockusb://1/2".to_string(), "rockusb://1/3".to_string()];
        assert_eq!(
            select_by_path("rockusb://1/3", &c, String::clone).unwrap(),
            "rockusb://1/3"
        );
        assert!(select_by_path("rockusb://1/4", &c, String::clone)
            .unwrap_err()
            .starts_with(TAG_ROCKUSB_DEVICE_NOT_FOUND));
        let dup = vec!["rockusb://1/2".to_string(), "rockusb://1/2".to_string()];
        assert!(select_by_path("rockusb://1/2", &dup, String::clone)
            .unwrap_err()
            .starts_with(TAG_ROCKUSB_MULTIPLE_DEVICES));
    }

    #[test]
    fn non_rockusb_paths_are_refused() {
        let c = vec!["rockusb://1/2".to_string()];
        for path in ["/dev/disk4", "qdl://1/2", "devsim://x", "ROCKUSB://1/2", ""] {
            assert!(select_by_path(path, &c, String::clone)
                .unwrap_err()
                .starts_with(TAG_INVALID_PATH));
        }
    }
}
