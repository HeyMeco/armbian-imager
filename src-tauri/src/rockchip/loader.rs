// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (c) 2026 Daniele Briguglio, superkali@armbian.com

//! Fetches and parses the rkbin SPL loader a board in Maskrom needs (DRAM init in 0x471, usbplug
//! in 0x472). Downloaded on first use from a pinned armbian/rkbin commit, never bundled: the blobs
//! are Rockchip-licensed binaries. Every copy, cached or fresh, must match the pinned SHA-256.

use std::path::PathBuf;

use rockfile::boot::{RkBootEntry, RkBootHeader, RkBootHeaderEntry};

use super::socs::RockchipSoc;
use super::TAG_ROCKUSB_LOADER_UNAVAILABLE;
use crate::config;
use crate::utils::{fetch_to_file, loaders_dir};
use crate::{log_info, log_warn};

/// armbian/rkbin commit the loader digests in `socs::SOCS` were taken from.
pub const RKBIN_COMMIT: &str = "30eb694ecb4afa1ca3ec1e4c5e477a81f7004e7e";

/// Cache subdirectory of `loaders_dir()` (QDL families live beside it).
const CACHE_SUBDIR: &str = "rockchip";

const HEADER_BYTES: usize = 102;
const ENTRY_BYTES: usize = 57;

/// Vendor request codes the boot ROM takes the two loader stages on.
pub const AREA_DRAM_INIT: u16 = 0x471;
pub const AREA_USBPLUG: u16 = 0x472;

/// One blob to upload in Maskrom, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderEntry {
    pub area: u16,
    pub name: String,
    pub data: Vec<u8>,
    /// Pause the file asks for after this upload, in milliseconds
    pub delay_ms: u32,
}

fn loader_url(soc: &RockchipSoc) -> String {
    format!(
        "{}{RKBIN_COMMIT}/{}",
        config::urls::RKBIN_RAW_BASE,
        soc.loader.rel
    )
}

fn cache_path(soc: &RockchipSoc) -> PathBuf {
    let file = soc.loader.rel.rsplit('/').next().unwrap_or(soc.loader.rel);
    loaders_dir().join(CACHE_SUBDIR).join(file)
}

/// Debug builds only: load this boot file instead of the pinned one, to try a newer loader on hardware.
#[cfg(debug_assertions)]
const LOADER_OVERRIDE_ENV: &str = "ARMBIAN_ROCKUSB_LOADER";

/// A local loader named by `ARMBIAN_ROCKUSB_LOADER`; no pinned digest, but it must parse and name this SoC.
#[cfg(debug_assertions)]
fn override_loader(soc: &RockchipSoc) -> Option<Result<Vec<LoaderEntry>, String>> {
    let path = std::env::var_os(LOADER_OVERRIDE_ENV)?;
    log_warn!(
        "rockchip::loader",
        "{LOADER_OVERRIDE_ENV} set: using {} instead of the pinned loader",
        std::path::Path::new(&path).display()
    );
    Some(
        std::fs::read(&path)
            .map_err(|e| {
                format!("{TAG_ROCKUSB_LOADER_UNAVAILABLE} can't read {LOADER_OVERRIDE_ENV}: {e}")
            })
            .and_then(|bytes| {
                let (header, entries) = parse_loader(&bytes).map_err(|e| {
                    format!("{TAG_ROCKUSB_LOADER_UNAVAILABLE} {LOADER_OVERRIDE_ENV}: {e}")
                })?;
                check_chip(&header, soc).map_err(|e| {
                    format!("{TAG_ROCKUSB_LOADER_UNAVAILABLE} {LOADER_OVERRIDE_ENV}: {e}")
                })?;
                Ok(entries)
            }),
    )
}

/// The SoC's loader entries, from the cache or a fresh pinned download.
pub async fn ensure_loader(soc: &'static RockchipSoc) -> Result<Vec<LoaderEntry>, String> {
    #[cfg(debug_assertions)]
    if let Some(entries) = override_loader(soc) {
        return entries;
    }
    let path = cache_path(soc);
    if let Ok(bytes) = std::fs::read(&path) {
        match validate(&bytes, soc) {
            Ok(entries) => {
                log_info!(
                    "rockchip::loader",
                    "Using cached loader: {}",
                    path.display()
                );
                return Ok(entries);
            }
            Err(e) => log_warn!(
                "rockchip::loader",
                "Cached loader {} rejected, downloading again: {}",
                path.display(),
                e
            ),
        }
    }

    let url = loader_url(soc);
    log_info!("rockchip::loader", "Downloading loader: {}", url);
    fetch_to_file(&url, &path, |bytes| validate(bytes, soc).map(|_| ()))
        .await
        .map_err(|e| format!("{TAG_ROCKUSB_LOADER_UNAVAILABLE} {e}"))?;
    let bytes = std::fs::read(&path).map_err(|e| {
        format!("{TAG_ROCKUSB_LOADER_UNAVAILABLE} Failed to read cached loader: {e}")
    })?;
    validate(&bytes, soc)
}

/// Pinned digest first, then the header must name this SoC and carry both stages.
fn validate(bytes: &[u8], soc: &RockchipSoc) -> Result<Vec<LoaderEntry>, String> {
    crate::qdl::registry::verify_digest(bytes, Some(soc.loader.sha256))?;
    let (header, entries) = parse_loader(bytes)?;
    check_chip(&header, soc)?;
    Ok(entries)
}

fn check_chip(header: &RkBootHeader, soc: &RockchipSoc) -> Result<(), String> {
    if header.supported_chip != soc.chip_tag {
        return Err(format!(
            "loader is for chip {:?}, not {}",
            String::from_utf8_lossy(&header.supported_chip),
            soc.name
        ));
    }
    Ok(())
}

/// Split an rkbin boot file ("BOOT"/"LDR " header) into its 0x471 and 0x472 uploads.
pub fn parse_loader(bytes: &[u8]) -> Result<(RkBootHeader, Vec<LoaderEntry>), String> {
    let head: &[u8; HEADER_BYTES] = bytes
        .get(..HEADER_BYTES)
        .and_then(|b| b.try_into().ok())
        .ok_or("loader is shorter than its header")?;
    let header = RkBootHeader::from_bytes(head).ok_or("not a Rockchip boot file")?;

    let mut entries = read_entries(bytes, &header.entry_471, AREA_DRAM_INIT)?;
    let usbplug = read_entries(bytes, &header.entry_472, AREA_USBPLUG)?;
    if entries.is_empty() || usbplug.is_empty() {
        return Err("loader lacks a DRAM init (0x471) or usbplug (0x472) stage".into());
    }
    entries.extend(usbplug);
    Ok((header, entries))
}

fn read_entries(
    bytes: &[u8],
    table: &RkBootHeaderEntry,
    area: u16,
) -> Result<Vec<LoaderEntry>, String> {
    let slice = |offset: u64, len: u64| -> Result<&[u8], String> {
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= bytes.len() as u64)
            .ok_or_else(|| format!("loader entry for {area:#x} runs past the end of the file"))?;
        Ok(&bytes[offset as usize..end as usize])
    };

    (0..table.count as u64)
        .map(|i| {
            let at = table.offset as u64 + table.size as u64 * i;
            let raw: &[u8; ENTRY_BYTES] = slice(at, ENTRY_BYTES as u64)?
                .try_into()
                .map_err(|_| "short loader entry".to_string())?;
            let entry = RkBootEntry::from_bytes(raw);
            let name = String::from_utf16_lossy(&entry.name)
                .trim_end_matches('\0')
                .to_string();
            let data = slice(entry.data_offset as u64, entry.data_size as u64)?;
            if data.is_empty() {
                return Err(format!("loader entry {name:?} is empty"));
            }
            Ok(LoaderEntry {
                area,
                name,
                data: data.to_vec(),
                delay_ms: entry.data_delay,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal boot file: one 0x471 and one 0x472 entry, laid out as boot_merger writes it.
    pub(crate) fn sample_loader(chip: &[u8; 4], sram: &[u8], ddr: &[u8]) -> Vec<u8> {
        let table_at = HEADER_BYTES as u32;
        let data_at = table_at + 2 * ENTRY_BYTES as u32;

        let mut out = Vec::new();
        out.extend_from_slice(b"LDR ");
        out.extend_from_slice(&(HEADER_BYTES as u16).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // version
        out.extend_from_slice(&0u32.to_le_bytes()); // merge version
        out.extend_from_slice(&[0u8; 7]); // release time
        out.extend_from_slice(&[chip[3], chip[2], chip[1], chip[0]]);
        for (i, _) in [sram, ddr].iter().enumerate() {
            out.push(1);
            out.extend_from_slice(&(table_at + i as u32 * ENTRY_BYTES as u32).to_le_bytes());
            out.push(ENTRY_BYTES as u8);
        }
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // no boot loader entries
        out.push(0); // sign flag
        out.push(1); // rc4 flag
        out.resize(HEADER_BYTES, 0);

        let mut offset = data_at;
        for (name, data) in [("sram", sram), ("ddr", ddr)] {
            let mut entry = vec![ENTRY_BYTES as u8];
            entry.extend_from_slice(&1u32.to_le_bytes());
            let mut name16: Vec<u16> = name.encode_utf16().collect();
            name16.resize(20, 0);
            for c in name16 {
                entry.extend_from_slice(&c.to_le_bytes());
            }
            entry.extend_from_slice(&offset.to_le_bytes());
            entry.extend_from_slice(&(data.len() as u32).to_le_bytes());
            entry.extend_from_slice(&5u32.to_le_bytes()); // delay
            entry.resize(ENTRY_BYTES, 0);
            out.extend_from_slice(&entry);
            offset += data.len() as u32;
        }
        out.extend_from_slice(sram);
        out.extend_from_slice(ddr);
        out
    }

    #[test]
    fn parses_both_stages_in_upload_order() {
        let file = sample_loader(b"3588", b"dram-init", b"usbplug!");
        let (header, entries) = parse_loader(&file).unwrap();
        assert_eq!(header.supported_chip, *b"3588");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].area, AREA_DRAM_INIT);
        assert_eq!(entries[0].name, "sram");
        assert_eq!(entries[0].data, b"dram-init");
        assert_eq!(entries[0].delay_ms, 5);
        assert_eq!(entries[1].area, AREA_USBPLUG);
        assert_eq!(entries[1].data, b"usbplug!");
    }

    #[test]
    fn truncated_or_foreign_files_are_refused() {
        let file = sample_loader(b"3588", b"dram-init", b"usbplug!");
        assert!(parse_loader(&file[..50]).is_err());
        assert!(parse_loader(&file[..file.len() - 3]).is_err());
        let mut bad = file.clone();
        bad[..4].copy_from_slice(b"ELF\0");
        assert!(parse_loader(&bad).is_err());
    }

    #[test]
    fn a_digest_mismatch_is_refused_before_parsing() {
        let soc = crate::rockchip::socs::find_by_maskrom_pid(0x350b).unwrap();
        let file = sample_loader(b"3588", b"dram-init", b"usbplug!");
        assert!(validate(&file, soc)
            .unwrap_err()
            .contains("SHA-256 mismatch"));
    }

    #[test]
    fn urls_point_at_the_pinned_commit() {
        let soc = crate::rockchip::socs::find_by_maskrom_pid(0x350b).unwrap();
        assert_eq!(
            loader_url(soc),
            format!(
                "https://raw.githubusercontent.com/armbian/rkbin/{RKBIN_COMMIT}/rk35/rk3588_spl_loader_v1.16.113.bin"
            )
        );
        assert!(cache_path(soc).ends_with("rockchip/rk3588_spl_loader_v1.16.113.bin"));
    }

    // Downloads every pinned loader and checks digest, header and chip tag:
    // `cargo test pinned_loaders_download_and_validate -- --ignored`.
    #[tokio::test]
    #[ignore = "downloads from raw.githubusercontent.com"]
    async fn pinned_loaders_download_and_validate() {
        for soc in crate::rockchip::socs::SOCS {
            let entries = ensure_loader(soc)
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", soc.name));
            assert!(entries.iter().any(|e| e.area == AREA_DRAM_INIT));
            assert!(entries.iter().any(|e| e.area == AREA_USBPLUG));
        }
    }
}
