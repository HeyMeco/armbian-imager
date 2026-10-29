// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (c) 2026 Daniele Briguglio, superkali@armbian.com

//! Fetches and assembles the rkbin loader a board in Maskrom needs (DRAM init in 0x471, usbplug in
//! 0x472): a merged boot file, or the two blobs behind a generated RKNS header. Downloaded on first
//! use from pinned rkbin commits, never bundled: the blobs are Rockchip-licensed binaries. Every
//! copy, cached or fresh, must match the pinned SHA-256.

use std::path::PathBuf;

use rockfile::boot::{RkBootEntry, RkBootHeader, RkBootHeaderEntry};

use sha2::{Digest, Sha256};

use super::socs::{LoaderBlob, LoaderSource, Rkbin, RockchipSoc};
use super::TAG_ROCKUSB_LOADER_UNAVAILABLE;
use crate::config;
use crate::utils::{fetch_to_file, loaders_dir};
use crate::{log_info, log_warn};

/// armbian/rkbin commit the Armbian digests in `socs::SOCS` were taken from.
pub const RKBIN_COMMIT: &str = "30eb694ecb4afa1ca3ec1e4c5e477a81f7004e7e";

/// rockchip-linux/rkbin commit the Rockchip digests in `socs::SOCS` were taken from.
pub const ROCKCHIP_RKBIN_COMMIT: &str = "3e288fe814e059dd06833495f845cab04ac20a5c";

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

fn blob_url(blob: &LoaderBlob) -> String {
    let (base, commit) = match blob.repo {
        Rkbin::Armbian => (config::urls::RKBIN_RAW_BASE, RKBIN_COMMIT),
        Rkbin::RockchipLinux => (config::urls::ROCKCHIP_RKBIN_RAW_BASE, ROCKCHIP_RKBIN_COMMIT),
    };
    format!("{base}{commit}/{}", blob.rel)
}

fn cache_path(blob: &LoaderBlob) -> PathBuf {
    let file = blob.rel.rsplit('/').next().unwrap_or(blob.rel);
    loaders_dir().join(CACHE_SUBDIR).join(file)
}

/// The pinned file's bytes, from the cache or a fresh download; both must match its SHA-256.
async fn fetch_blob(blob: &LoaderBlob) -> Result<Vec<u8>, String> {
    let path = cache_path(blob);
    let check = |bytes: &[u8]| crate::qdl::registry::verify_digest(bytes, Some(blob.sha256));
    if let Ok(bytes) = std::fs::read(&path) {
        match check(&bytes) {
            Ok(()) => {
                log_info!("rockchip::loader", "Using cached {}", path.display());
                return Ok(bytes);
            }
            Err(e) => log_warn!(
                "rockchip::loader",
                "Cached {} rejected, downloading again: {}",
                path.display(),
                e
            ),
        }
    }
    let url = blob_url(blob);
    log_info!("rockchip::loader", "Downloading {}", url);
    fetch_to_file(&url, &path, check)
        .await
        .map_err(|e| format!("{TAG_ROCKUSB_LOADER_UNAVAILABLE} {e}"))?;
    let bytes = std::fs::read(&path).map_err(|e| {
        format!(
            "{TAG_ROCKUSB_LOADER_UNAVAILABLE} Failed to read {}: {e}",
            path.display()
        )
    })?;
    check(&bytes).map(|()| bytes)
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
    match &soc.loader {
        LoaderSource::Merged(blob) => {
            let bytes = fetch_blob(blob).await?;
            merged_entries(&bytes, soc).map_err(|e| format!("{TAG_ROCKUSB_LOADER_UNAVAILABLE} {e}"))
        }
        LoaderSource::Split { ddr, usbplug } => {
            let ddr = fetch_blob(ddr).await?;
            let usbplug = fetch_blob(usbplug).await?;
            Ok(split_entries(&ddr, &usbplug))
        }
    }
}

/// A merged boot file's stages, once its header names this SoC.
fn merged_entries(bytes: &[u8], soc: &RockchipSoc) -> Result<Vec<LoaderEntry>, String> {
    let (header, entries) = parse_loader(bytes)?;
    check_chip(&header, soc)?;
    Ok(entries)
}

/// boot_merger's alignment for new-IDB images (`ALIGN=8` sectors).
const RKNS_ALIGN: usize = 4096;
const RKNS_HEADER_BYTES: usize = 4096;
/// Header size field, as boot_merger writes it (0x180).
const RKNS_HEADER_SIZE_FIELD: u32 = 0x180;
const RKNS_IMAGE_ENTRIES_AT: usize = 120;
const RKNS_IMAGE_ENTRY_BYTES: usize = 88;
const RKNS_IMAGE_HASH_AT: usize = 24;
const RKNS_HEADER_HASH_AT: usize = 1536;
/// Byte 97: 0 in boot_merger output before late 2024, 1 since; undocumented, so the newer value.
const RKNS_FLAG_AT: usize = 97;

fn pad_to(data: &[u8], align: usize) -> Vec<u8> {
    let mut out = data.to_vec();
    out.resize(data.len().div_ceil(align) * align, 0);
    out
}

/// The RKNS ("UsbHead") header boot_merger puts in front of new-IDB 0x471 uploads: each image's
/// sector size and offset, a SHA-256 per image, and a SHA-256 over the header. Checked byte for
/// byte against the armbian/rkbin RK3576 v1.03 and Radxa v1.09 merged loaders.
pub fn rkns_header(images: &[&[u8]], flag: u8) -> Vec<u8> {
    let mut h = vec![0u8; RKNS_HEADER_BYTES];
    h[..4].copy_from_slice(b"RKNS");
    h[8..12]
        .copy_from_slice(&(RKNS_HEADER_SIZE_FIELD | ((images.len() as u32) << 16)).to_le_bytes());
    h[12..16].copy_from_slice(&1u32.to_le_bytes()); // SHA-256 digests
    h[RKNS_FLAG_AT] = flag;
    let mut offset = (RKNS_HEADER_BYTES / 512) as u32;
    for (k, image) in images.iter().enumerate() {
        let sectors = (image.len() / 512) as u32;
        let at = RKNS_IMAGE_ENTRIES_AT + RKNS_IMAGE_ENTRY_BYTES * k;
        h[at..at + 4].copy_from_slice(&((sectors << 16) | offset).to_le_bytes());
        h[at + 4..at + 8].copy_from_slice(&u32::MAX.to_le_bytes()); // no load address
        h[at + 12..at + 16].copy_from_slice(&(k as u32 + 1).to_le_bytes());
        h[at + RKNS_IMAGE_HASH_AT..at + RKNS_IMAGE_HASH_AT + 32]
            .copy_from_slice(&Sha256::digest(image));
        offset += sectors;
    }
    let digest = Sha256::digest(&h[..RKNS_HEADER_HASH_AT]);
    h[RKNS_HEADER_HASH_AT..RKNS_HEADER_HASH_AT + 32].copy_from_slice(&digest);
    h
}

/// The upload boot_merger would produce from the two blobs: header + DDR init on 0x471, usbplug on 0x472.
fn split_entries(ddr: &[u8], usbplug: &[u8]) -> Vec<LoaderEntry> {
    let ddr = pad_to(ddr, RKNS_ALIGN);
    let usbplug = pad_to(usbplug, RKNS_ALIGN);
    let head = rkns_header(&[&ddr, &usbplug], 1);
    vec![
        LoaderEntry {
            area: AREA_DRAM_INIT,
            name: "UsbHead".into(),
            data: head,
            delay_ms: 1,
        },
        LoaderEntry {
            area: AREA_DRAM_INIT,
            name: "ddr".into(),
            data: ddr,
            delay_ms: 1,
        },
        LoaderEntry {
            area: AREA_USBPLUG,
            name: "usbplug".into(),
            data: usbplug,
            delay_ms: 0,
        },
    ]
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
    fn a_merged_loader_must_name_the_soc() {
        let rk3588 = crate::rockchip::socs::find_by_maskrom_pid(0x350b).unwrap();
        let rk3399 = crate::rockchip::socs::find_by_maskrom_pid(0x330c).unwrap();
        let file = sample_loader(b"3588", b"dram-init", b"usbplug!");
        assert!(merged_entries(&file, rk3588).is_ok());
        assert!(merged_entries(&file, rk3399)
            .unwrap_err()
            .contains("not RK3399"));
    }

    #[test]
    fn urls_point_at_the_pinned_commits() {
        let LoaderSource::Merged(rk3588) = &crate::rockchip::socs::find_by_maskrom_pid(0x350b)
            .unwrap()
            .loader
        else {
            panic!("RK3588 uses a merged loader");
        };
        assert_eq!(
            blob_url(rk3588),
            format!("https://raw.githubusercontent.com/armbian/rkbin/{RKBIN_COMMIT}/rk35/rk3588_spl_loader_v1.16.113.bin")
        );
        assert!(cache_path(rk3588).ends_with("rockchip/rk3588_spl_loader_v1.16.113.bin"));

        let LoaderSource::Split { usbplug, .. } =
            &crate::rockchip::socs::find_by_maskrom_pid(0x350e)
                .unwrap()
                .loader
        else {
            panic!("RK3576 uses split blobs");
        };
        assert_eq!(
            blob_url(usbplug),
            format!("https://raw.githubusercontent.com/rockchip-linux/rkbin/{ROCKCHIP_RKBIN_COMMIT}/bin/rk35/rk3576_usbplug_v1.04.bin")
        );
    }

    #[test]
    fn rkns_header_describes_each_padded_image() {
        let ddr = pad_to(&[1u8; 5000], RKNS_ALIGN);
        let plug = pad_to(&[2u8; 9000], RKNS_ALIGN);
        assert_eq!((ddr.len(), plug.len()), (8192, 12288));
        let h = rkns_header(&[&ddr, &plug], 1);
        let u32_at = |at: usize| u32::from_le_bytes(h[at..at + 4].try_into().unwrap());

        assert_eq!(h.len(), 4096);
        assert_eq!(&h[..4], b"RKNS");
        assert_eq!(u32_at(8), 0x0002_0180);
        assert_eq!(u32_at(12), 1);
        assert_eq!(h[RKNS_FLAG_AT], 1);
        // DDR at sector 8 (after the header), 16 sectors; usbplug right behind it, 24 sectors.
        assert_eq!(u32_at(120), (16 << 16) | 8);
        assert_eq!(u32_at(208), (24 << 16) | 24);
        assert_eq!(u32_at(124), u32::MAX);
        assert_eq!((u32_at(132), u32_at(220)), (1, 2));
        assert_eq!(&h[144..176], Sha256::digest(&ddr).as_slice());
        assert_eq!(&h[232..264], Sha256::digest(&plug).as_slice());
        assert_eq!(&h[1536..1568], Sha256::digest(&h[..1536]).as_slice());
        assert!(h[1568..].iter().all(|b| *b == 0));
    }

    #[test]
    fn split_upload_is_header_ddr_then_usbplug() {
        let entries = split_entries(&[1u8; 100], &[2u8; 100]);
        let shape: Vec<(u16, &str, usize, u32)> = entries
            .iter()
            .map(|e| (e.area, e.name.as_str(), e.data.len(), e.delay_ms))
            .collect();
        assert_eq!(
            shape,
            [
                (AREA_DRAM_INIT, "UsbHead", 4096, 1),
                (AREA_DRAM_INIT, "ddr", 4096, 1),
                (AREA_USBPLUG, "usbplug", 4096, 0)
            ]
        );
    }

    // Rebuilds the armbian/rkbin RK3576 v1.03 merged loader's upload from its raw blobs and compares
    // byte for byte (the older boot_merger wrote flag 0): `cargo test split_upload_matches -- --ignored`.
    #[tokio::test]
    #[ignore = "downloads from raw.githubusercontent.com"]
    async fn split_upload_matches_boot_merger_output() {
        let get = |rel: &'static str, sha256: &'static str| LoaderBlob {
            repo: Rkbin::Armbian,
            rel,
            sha256,
        };
        let merged = fetch_blob(&get(
            "rk35/rk3576_spl_loader_v1.03.102.bin",
            "744af15f17fbd0849bbd75ce71f4fa7834d55ffb2acae8a05d3aaf95b8754aff",
        ))
        .await
        .unwrap();
        let ddr = fetch_blob(&get(
            "rk35/rk3576_ddr_lp4_2112MHz_lp5_2736MHz_v1.03.bin",
            "93fe6770ce70214cda882d4360da5485bb998f98871df3f070af62e33d29cf23",
        ))
        .await;
        let (_, expected) = parse_loader(&merged).unwrap();
        let ddr = ddr.unwrap();
        let usbplug = fetch_blob(&get(
            "rk35/rk3576_usbplug_v1.01.bin",
            "b73c4c93f6ab749cd9788f4f670b636ba4b6b1be5f6bc75e4f3ad549df4b7984",
        ))
        .await
        .unwrap();
        let mut built = split_entries(&ddr, &usbplug);
        built[0].data = rkns_header(&[&built[1].data, &built[2].data], 0);
        for (b, e) in built.iter().zip(&expected) {
            assert_eq!(
                (b.area, b.data.len(), b.delay_ms),
                (e.area, e.data.len(), e.delay_ms),
                "{}",
                e.name
            );
            assert!(b.data == e.data, "{} differs", e.name);
        }
    }

    // Downloads every pinned loader blob and checks digests, headers and chip tags:
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
