// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (c) 2026 Daniele Briguglio, superkali@armbian.com

//! Rockchip board flashing over USB (rockusb protocol), as an alternative to writing a disk.
//! A board in Maskrom mode first gets an rkbin SPL loader (DRAM init + usbplug) uploaded; a board
//! in Loader mode (usbplug or U-Boot `rockusb`) takes LBA writes to its storage directly.

pub mod detect;
pub mod flash;
pub mod loader;
pub mod socs;

use serde::{Deserialize, Serialize};

/// USB Vendor ID of Rockchip (Fuzhou Rockchip Electronics)
pub const ROCKCHIP_VID: u16 = 0x2207;

/// Scheme of the device path identifying one Rockchip USB device (`rockusb://<bus_id>/<port_chain>`).
/// Keyed by physical port, not address, so a board keeps its path when Maskrom re-enumerates as Loader.
pub const ROCKUSB_PATH_PREFIX: &str = "rockusb://";

pub const TAG_ROCKUSB_DEVICE_NOT_FOUND: &str = "[ROCKUSB_DEVICE_NOT_FOUND]";
pub const TAG_ROCKUSB_MULTIPLE_DEVICES: &str = "[ROCKUSB_MULTIPLE_DEVICES]";
pub const TAG_ROCKUSB_PERMISSION_DENIED: &str = "[ROCKUSB_PERMISSION_DENIED]";
pub const TAG_ROCKUSB_CONNECTION_FAILED: &str = "[ROCKUSB_CONNECTION_FAILED]";
/// The OS driver bound to the device isn't one nusb can use (Windows without WinUSB)
pub const TAG_ROCKUSB_DRIVER_MISSING: &str = "[ROCKUSB_DRIVER_MISSING]";
pub const TAG_ROCKUSB_DISCONNECTED: &str = "[ROCKUSB_DISCONNECTED]";
pub const TAG_ROCKUSB_TIMEOUT: &str = "[ROCKUSB_TIMEOUT]";
pub const TAG_ROCKUSB_UNSUPPORTED_SOC: &str = "[ROCKUSB_UNSUPPORTED_SOC]";
pub const TAG_ROCKUSB_SOC_MISMATCH: &str = "[ROCKUSB_SOC_MISMATCH]";
pub const TAG_ROCKUSB_LOADER_UNAVAILABLE: &str = "[ROCKUSB_LOADER_UNAVAILABLE]";
pub const TAG_ROCKUSB_LOADER_FAILED: &str = "[ROCKUSB_LOADER_FAILED]";
pub const TAG_ROCKUSB_NO_LOADER_MODE: &str = "[ROCKUSB_NO_LOADER_MODE]";
pub const TAG_ROCKUSB_STORAGE_UNAVAILABLE: &str = "[ROCKUSB_STORAGE_UNAVAILABLE]";
pub const TAG_ROCKUSB_ERROR: &str = "[ROCKUSB_ERROR]";

/// `FlashState.board.stage` values; twin of BOARD_STAGE in src/config/constants.ts.
pub const STAGE_CONNECTING: &str = "connecting";
pub const STAGE_LOADER: &str = "loader";
pub const STAGE_RECONNECTING: &str = "reconnecting";
pub const STAGE_STORAGE: &str = "storage";
pub const STAGE_WRITING: &str = "writing";
pub const STAGE_VERIFYING: &str = "verifying";
pub const STAGE_RESETTING: &str = "resetting";
pub const STAGE_COMPLETE: &str = "complete";

/// What the board's USB stack is running, from bit 0 of `bcdUSB` (as rkdeveloptool and xrock read it).
/// Only a hint: some loaders (RK3576 usbplug) keep the boot ROM's descriptor, so the flash asks the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RockchipMode {
    /// The boot ROM: only accepts a loader upload over control transfers.
    Maskrom,
    /// usbplug or U-Boot rockusb: the full protocol, LBA reads and writes included.
    Loader,
}

impl RockchipMode {
    pub fn from_bcd_usb(bcd_usb: u16) -> Self {
        if bcd_usb & 0x1 == 0 {
            RockchipMode::Maskrom
        } else {
            RockchipMode::Loader
        }
    }
}

/// Storage on the board the image is written to; the wire value is the rockusb `StorageIndex`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoardStorage {
    Emmc,
    Sd,
    Spinor,
    Ufs,
}

impl BoardStorage {
    /// Every storage the flash can target, in the order a probe tries them.
    pub const ALL: [BoardStorage; 4] = [
        BoardStorage::Emmc,
        BoardStorage::Sd,
        BoardStorage::Spinor,
        BoardStorage::Ufs,
    ];

    /// Index for `switch_storage`; eMMC, SD and SPI NOR are the three rkdeveloptool's `cs` offers.
    pub fn index(self) -> ::rockusb::protocol::StorageIndex {
        use ::rockusb::protocol::StorageIndex;
        match self {
            BoardStorage::Emmc => StorageIndex::Emmc,
            BoardStorage::Sd => StorageIndex::Sd0,
            BoardStorage::Spinor => StorageIndex::MtdBlkSpiNor,
            BoardStorage::Ufs => StorageIndex::Ufs,
        }
    }

    /// Whether a storage readback means this storage is selected. Rockchip's rockusb gadget
    /// (f_rockusb.c) switches SATA and UFS to the same SCSI device and reports it as SATA unless
    /// built with CONFIG_UFS and a 4 KiB block size, so a UFS board may read back as SATA.
    pub fn is_reported_as(self, reported: ::rockusb::protocol::StorageIndex) -> bool {
        use ::rockusb::protocol::StorageIndex;
        reported == self.index() || (self == BoardStorage::Ufs && reported == StorageIndex::Sata)
    }
}

/// The storage the user asked for: one in particular, or the largest the board's loader finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageChoice {
    Auto,
    Emmc,
    Sd,
    Spinor,
    Ufs,
}

impl StorageChoice {
    /// The storage named outright; None for `Auto`, resolved once the loader runs.
    pub fn fixed(self) -> Option<BoardStorage> {
        match self {
            StorageChoice::Auto => None,
            StorageChoice::Emmc => Some(BoardStorage::Emmc),
            StorageChoice::Sd => Some(BoardStorage::Sd),
            StorageChoice::Spinor => Some(BoardStorage::Spinor),
            StorageChoice::Ufs => Some(BoardStorage::Ufs),
        }
    }
}

/// One storage a running loader can reach, with its size
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageInfo {
    pub storage: BoardStorage,
    pub size: u64,
}

/// The biggest storage found; on a tie the earlier one in `BoardStorage::ALL` order.
pub fn largest_storage(found: &[StorageInfo]) -> Option<BoardStorage> {
    found
        .iter()
        .fold(None::<&StorageInfo>, |best, s| match best {
            Some(b) if b.size >= s.size => Some(b),
            _ => Some(s),
        })
        .map(|s| s.storage)
}

/// A Rockchip device connected over USB in Maskrom or Loader mode
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RockchipDevice {
    /// Device path the frontend passes back to target this device
    pub path: String,
    pub mode: RockchipMode,
    pub product_id: u16,
    /// SoC name ("RK3588") when the boot ROM PID identifies it; Loader PIDs are shared across SoCs
    pub soc: Option<String>,
    /// Whether this build can bring the board from Maskrom to Loader (a pinned loader exists)
    pub maskrom_supported: bool,
    pub bus_id: String,
    pub port_chain: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::rockusb::protocol::StorageIndex;

    #[test]
    fn ufs_accepts_the_shared_scsi_readback() {
        assert!(BoardStorage::Ufs.is_reported_as(StorageIndex::Ufs));
        assert!(BoardStorage::Ufs.is_reported_as(StorageIndex::Sata));
        assert!(!BoardStorage::Ufs.is_reported_as(StorageIndex::Emmc));
    }

    #[test]
    fn the_largest_storage_wins_and_ties_keep_probe_order() {
        let found = |v: &[(BoardStorage, u64)]| -> Vec<StorageInfo> {
            v.iter()
                .map(|&(storage, size)| StorageInfo { storage, size })
                .collect()
        };
        assert_eq!(largest_storage(&[]), None);
        assert_eq!(
            largest_storage(&found(&[
                (BoardStorage::Emmc, 32 << 30),
                (BoardStorage::Spinor, 16 << 20),
                (BoardStorage::Ufs, 128 << 30),
            ])),
            Some(BoardStorage::Ufs)
        );
        assert_eq!(
            largest_storage(&found(&[(BoardStorage::Emmc, 1), (BoardStorage::Ufs, 1)])),
            Some(BoardStorage::Emmc)
        );
    }

    #[test]
    fn auto_has_no_fixed_storage() {
        assert_eq!(StorageChoice::Auto.fixed(), None);
        assert_eq!(StorageChoice::Ufs.fixed(), Some(BoardStorage::Ufs));
    }

    #[test]
    fn other_storages_need_their_own_index() {
        assert!(BoardStorage::Emmc.is_reported_as(StorageIndex::Emmc));
        assert!(!BoardStorage::Emmc.is_reported_as(StorageIndex::Sata));
        assert!(!BoardStorage::Sd.is_reported_as(StorageIndex::Emmc));
        assert!(BoardStorage::Spinor.is_reported_as(StorageIndex::MtdBlkSpiNor));
    }
}
