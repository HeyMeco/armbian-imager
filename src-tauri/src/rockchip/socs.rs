// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (c) 2026 Daniele Briguglio, superkali@armbian.com

//! Rockchip SoCs this build can flash from Maskrom: the boot ROM USB PID that identifies each one,
//! and the pinned rkbin SPL loader that brings it up to Loader mode. Loader mode itself needs no
//! entry here; any SoC already running usbplug or U-Boot rockusb can be written.

/// The rkbin repository a blob is pinned in; each has its own commit in `loader`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rkbin {
    Armbian,
    RockchipLinux,
}

/// One pinned rkbin file.
pub struct LoaderBlob {
    pub repo: Rkbin,
    /// Path inside the rkbin repo
    pub rel: &'static str,
    /// SHA-256 of the file at the pinned commit; a download that differs is refused
    pub sha256: &'static str,
}

/// How a SoC's Maskrom upload is put together.
pub enum LoaderSource {
    /// A merged boot file (boot_merger output) carrying both stages.
    Merged(LoaderBlob),
    /// The DDR init and usbplug blobs, uploaded behind a generated RKNS header the way boot_merger
    /// lays them out (RK35xx new-IDB boot ROMs).
    Split {
        ddr: LoaderBlob,
        usbplug: LoaderBlob,
    },
}

#[cfg(test)]
impl LoaderSource {
    pub fn blobs(&self) -> Vec<&LoaderBlob> {
        match self {
            LoaderSource::Merged(blob) => vec![blob],
            LoaderSource::Split { ddr, usbplug } => vec![ddr, usbplug],
        }
    }
}

pub struct RockchipSoc {
    /// Display name, also the canonical SoC token
    pub name: &'static str,
    /// SoC names this entry covers, matched against the board's API `soc` field
    pub tokens: &'static [&'static str],
    /// USB product ID the boot ROM enumerates with in Maskrom
    pub maskrom_pid: u16,
    /// Chip tag as the loader header's `supported_chip` carries it (and, read the same way, ChipInfo)
    pub chip_tag: [u8; 4],
    pub loader: LoaderSource,
}

/// Maskrom PIDs from xrock's chip table (RK3528 from `lsusb` of a board in Maskrom); loaders and
/// digests from the pinned rkbin commits. RK3576 temporarily takes rockchip-linux blobs: the merged
/// loader in armbian/rkbin carries usbplug v1.01, whose UFS commands past 32 MiB fail silently.
pub const SOCS: &[RockchipSoc] = &[
    RockchipSoc {
        name: "RK3588",
        tokens: &["RK3588", "RK3582", "RK3583"],
        maskrom_pid: 0x350b,
        chip_tag: *b"3588",
        loader: LoaderSource::Merged(LoaderBlob {
            repo: Rkbin::Armbian,
            rel: "rk35/rk3588_spl_loader_v1.16.113.bin",
            sha256: "4cc43c2ff29e08b5491b4d52528346aa7da6948128c17e670ff8a000029c9408",
        }),
    },
    RockchipSoc {
        name: "RK3566/RK3568",
        tokens: &["RK3566", "RK3568"],
        maskrom_pid: 0x350a,
        chip_tag: *b"3568",
        loader: LoaderSource::Merged(LoaderBlob {
            repo: Rkbin::Armbian,
            rel: "rk35/rk356x_spl_loader_v1.21.113.bin",
            sha256: "aaa3f13c84275bb864e78b5dec29fcce43dec2898ecac6696a06f14a3dec679e",
        }),
    },
    RockchipSoc {
        name: "RK3576",
        tokens: &["RK3576"],
        maskrom_pid: 0x350e,
        chip_tag: *b"3576",
        loader: LoaderSource::Split {
            ddr: LoaderBlob {
                repo: Rkbin::RockchipLinux,
                rel: "bin/rk35/rk3576_ddr_lp4_2112MHz_lp5_2736MHz_v1.13.bin",
                sha256: "f697151bdd09d31bf7a1e49d63796a6a200e81bf8f9a766c2e95c840e88fc0ce",
            },
            usbplug: LoaderBlob {
                repo: Rkbin::RockchipLinux,
                rel: "bin/rk35/rk3576_usbplug_v1.04.bin",
                sha256: "77238d47f10e84844d95707024b8171fc9f198e19ad6500f5e8a5810ca6c680b",
            },
        },
    },
    RockchipSoc {
        name: "RK3528",
        tokens: &["RK3528"],
        maskrom_pid: 0x350c,
        chip_tag: *b"3528",
        loader: LoaderSource::Merged(LoaderBlob {
            repo: Rkbin::Armbian,
            rel: "rk35/rk3528_spl_loader_v1.07.104.bin",
            sha256: "a45957495e21136736f312b94ada0f859ec4fad432f2810283da35f027ec41ca",
        }),
    },
    RockchipSoc {
        name: "RK3399",
        tokens: &["RK3399"],
        maskrom_pid: 0x330c,
        chip_tag: *b"330C",
        loader: LoaderSource::Merged(LoaderBlob {
            repo: Rkbin::Armbian,
            rel: "rk33/rk3399_loader_v1.30.130.bin",
            sha256: "c3333e65e2262f91880d62c76d25f4f47dc0f127c96d2df25e3400a826479118",
        }),
    },
    RockchipSoc {
        name: "RK3328",
        tokens: &["RK3328"],
        maskrom_pid: 0x320c,
        chip_tag: *b"322H",
        loader: LoaderSource::Merged(LoaderBlob {
            repo: Rkbin::Armbian,
            rel: "rk33/rk3328_loader_ddr333_v1.08.244.bin",
            sha256: "c343691af5c5099e332a02732727e572f2239621ba92e5191bb39d4cb9896a7b",
        }),
    },
];

pub fn find_by_maskrom_pid(pid: u16) -> Option<&'static RockchipSoc> {
    SOCS.iter().find(|s| s.maskrom_pid == pid)
}

pub fn find_by_chip_tag(tag: [u8; 4]) -> Option<&'static RockchipSoc> {
    SOCS.iter().find(|s| s.chip_tag == tag)
}

fn normalize(soc: &str) -> String {
    soc.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// The table entry a board's API `soc` string names ("rk3588s", "Rockchip RK3566"), if any.
pub fn find_by_board_soc(board_soc: &str) -> Option<&'static RockchipSoc> {
    let soc = normalize(board_soc);
    if soc.is_empty() {
        return None;
    }
    SOCS.iter()
        .find(|s| s.tokens.iter().any(|token| soc.contains(token)))
}

/// Chip tag from a ChipInfo reply, byte order as the loader header stores it ("3588").
/// Unverified on hardware: a reply that doesn't decode simply leaves the SoC unknown.
pub fn chip_tag_from_info(info: &[u8]) -> Option<[u8; 4]> {
    let head: [u8; 4] = info.get(..4)?.try_into().ok()?;
    let tag = [head[3], head[2], head[1], head[0]];
    tag.iter().all(u8::is_ascii_alphanumeric).then_some(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maskrom_pids_and_chip_tags_are_unique() {
        for (i, a) in SOCS.iter().enumerate() {
            for b in &SOCS[i + 1..] {
                assert_ne!(a.maskrom_pid, b.maskrom_pid, "{} / {}", a.name, b.name);
                assert_ne!(a.chip_tag, b.chip_tag, "{} / {}", a.name, b.name);
            }
        }
    }

    #[test]
    fn every_loader_digest_is_a_sha256() {
        for soc in SOCS {
            for blob in soc.loader.blobs() {
                assert_eq!(blob.sha256.len(), 64, "{}", soc.name);
                assert!(hex::decode(blob.sha256).is_ok(), "{}", soc.name);
                assert!(blob.rel.ends_with(".bin"), "{}", soc.name);
            }
        }
    }

    #[test]
    fn board_soc_strings_resolve_to_their_family() {
        assert_eq!(find_by_board_soc("rk3588").unwrap().name, "RK3588");
        assert_eq!(find_by_board_soc("RK3588S").unwrap().name, "RK3588");
        assert_eq!(
            find_by_board_soc("Rockchip RK3566").unwrap().name,
            "RK3566/RK3568"
        );
        assert_eq!(find_by_board_soc("rk-3568").unwrap().name, "RK3566/RK3568");
        assert_eq!(find_by_board_soc("RK3399").unwrap().name, "RK3399");
        assert!(find_by_board_soc("S922X").is_none());
        assert!(find_by_board_soc("").is_none());
    }

    #[test]
    fn maskrom_pid_names_the_soc() {
        assert_eq!(find_by_maskrom_pid(0x350b).unwrap().name, "RK3588");
        assert_eq!(find_by_maskrom_pid(0x330c).unwrap().name, "RK3399");
        assert!(find_by_maskrom_pid(0x330a).is_none());
    }

    #[test]
    fn chip_info_decodes_reversed_ascii() {
        let mut info = [0u8; 16];
        info[..4].copy_from_slice(b"8853");
        assert_eq!(chip_tag_from_info(&info), Some(*b"3588"));
        assert_eq!(find_by_chip_tag(*b"3588").unwrap().name, "RK3588");
        assert_eq!(chip_tag_from_info(&[0, 0, 0, 0]), None);
        assert_eq!(chip_tag_from_info(&[1, 2]), None);
    }
}
