// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (c) 2026 Daniele Briguglio, superkali@armbian.com

//! Rockchip board flash: bring a Maskrom board up to Loader mode with the SoC's SPL loader, select
//! the target storage, stream the image as LBA writes, read it back to verify, then reset the board.

use std::fs::File;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nusb::{DeviceInfo, ErrorKind};
use rockusb::device::Error as RockusbError;
use rockusb::nusb::{Device, DeviceUnavailable, TransferError};
use rockusb::protocol::{ResetOpcode, SECTOR_SIZE};

use super::socs::{self, RockchipSoc};
use super::{
    detect, loader, BoardStorage, RockchipMode, STAGE_COMPLETE, STAGE_CONNECTING, STAGE_LOADER,
    STAGE_RECONNECTING, STAGE_RESETTING, STAGE_STORAGE, STAGE_VERIFYING, STAGE_WRITING,
    TAG_ROCKUSB_CONNECTION_FAILED, TAG_ROCKUSB_DISCONNECTED, TAG_ROCKUSB_DRIVER_MISSING,
    TAG_ROCKUSB_ERROR, TAG_ROCKUSB_LOADER_FAILED, TAG_ROCKUSB_NO_LOADER_MODE,
    TAG_ROCKUSB_PERMISSION_DENIED, TAG_ROCKUSB_SOC_MISMATCH, TAG_ROCKUSB_STORAGE_UNAVAILABLE,
    TAG_ROCKUSB_TIMEOUT, TAG_ROCKUSB_UNSUPPORTED_SOC,
};
use crate::config::rockusb as cfg;
use crate::flash::verify::{
    check_verified_length, data_mismatch_err, short_read_err, verify_cancelled_err,
    TAG_VERIFY_READ_FAILED,
};
use crate::flash::{cancelled_err, check_capacity, open_image, write_failed_err, FlashState};
use crate::utils::tagged;
use crate::{log_info, log_warn};

const MODULE: &str = "rockchip::flash";

/// What the user asked for, beyond the image and device.
pub struct BoardFlashRequest<'a> {
    pub image_path: &'a Path,
    pub device_path: &'a str,
    pub storage: BoardStorage,
    pub verify: bool,
    /// API `soc` of the image's board, when known; a board with another SoC is refused
    pub board_soc: Option<&'a str>,
}

/// Flash `image_path` to the board at `device_path`, Maskrom or Loader.
pub async fn rockusb_flash(
    req: BoardFlashRequest<'_>,
    state: Arc<FlashState>,
) -> Result<(), String> {
    let image_family = req.board_soc.and_then(socs::find_by_board_soc);

    state.board.is_active.store(true, Ordering::SeqCst);
    set_stage(&state, STAGE_CONNECTING);
    let (info, found) = detect::resolve(req.device_path)?;
    log_info!(
        MODULE,
        "Board {:?}: PID {:#06x}, {:?} mode",
        req.device_path,
        found.product_id,
        found.mode
    );

    let info = match found.mode {
        RockchipMode::Loader => info,
        RockchipMode::Maskrom => {
            let soc = socs::find_by_maskrom_pid(found.product_id).ok_or_else(|| {
                format!(
                    "{TAG_ROCKUSB_UNSUPPORTED_SOC} no loader for Maskrom PID {:#06x}",
                    found.product_id
                )
            })?;
            check_same_family(image_family, soc)?;
            let entries = loader::ensure_loader(soc).await?;
            state.ensure_not_cancelled()?;

            set_stage(&state, STAGE_LOADER);
            upload_loader(&info, soc, &entries).await?;
            state.ensure_not_cancelled()?;

            set_stage(&state, STAGE_RECONNECTING);
            wait_for_loader_mode(req.device_path, &state).await?
        }
    };

    let mut device = open_with_retry(&info).await?;

    // The board reports its own chip in Loader mode; a different known SoC means the wrong image.
    match with_timeout(device.chip_info()).await {
        Ok(Ok(chip)) => {
            let tag = socs::chip_tag_from_info(chip.inner());
            log_info!(
                MODULE,
                "Chip info {:02x?} (tag {:?})",
                chip.inner(),
                tag.map(|t| String::from_utf8_lossy(&t).into_owned())
            );
            if let Some(soc) = tag.and_then(socs::find_by_chip_tag) {
                check_same_family(image_family, soc)?;
            }
        }
        other => log_warn!(MODULE, "Chip info unavailable: {:?}", other.err()),
    }

    state.ensure_not_cancelled()?;
    set_stage(&state, STAGE_STORAGE);
    select_storage(&mut device, req.storage).await?;

    let flash_info = with_timeout(device.flash_info())
        .await?
        .map_err(|e| usb_err("read the storage size", e))?;
    let capacity = flash_info.size();
    log_info!(
        MODULE,
        "{:?} selected: {} sectors ({} bytes)",
        req.storage,
        flash_info.sectors(),
        capacity
    );

    let (mut image, image_len) = open_image(req.image_path)?;
    check_capacity(image_len, capacity)?;
    state.ensure_not_cancelled()?;

    set_stage(&state, STAGE_WRITING);
    state.total_bytes.store(image_len, Ordering::SeqCst);
    write_image(&mut device, &mut image, image_len, &state).await?;

    if req.verify {
        set_stage(&state, STAGE_VERIFYING);
        state.is_verifying.store(true, Ordering::SeqCst);
        verify_image(&mut device, &mut image, image_len, &state).await?;
    }

    set_stage(&state, STAGE_RESETTING);
    if let Err(e) = with_timeout(device.reset_device(ResetOpcode::Reset)).await {
        log_warn!(MODULE, "Board reset failed (non-fatal): {}", e);
    }
    set_stage(&state, STAGE_COMPLETE);
    log_info!(MODULE, "Board flash completed");
    Ok(())
}

fn set_stage(state: &FlashState, stage: &str) {
    *state.board.stage.lock().unwrap_or_else(|p| p.into_inner()) = stage.to_string();
}

fn check_same_family(image: Option<&RockchipSoc>, board: &RockchipSoc) -> Result<(), String> {
    match image {
        Some(image) if !std::ptr::eq(image, board) => Err(format!(
            "{TAG_ROCKUSB_SOC_MISMATCH} the image is for {}, the connected board is {}",
            image.name, board.name
        )),
        _ => Ok(()),
    }
}

/// Bound one USB command, so a hung board fails the flash instead of stalling it forever.
async fn with_timeout<T>(fut: impl Future<Output = T>) -> Result<T, String> {
    tokio::time::timeout(Duration::from_secs(cfg::COMMAND_TIMEOUT_SECS), fut)
        .await
        .map_err(|_| {
            format!(
                "{TAG_ROCKUSB_TIMEOUT} the board didn't answer within {}s",
                cfg::COMMAND_TIMEOUT_SECS
            )
        })
}

fn is_disconnect(e: &RockusbError<TransferError>) -> bool {
    matches!(e, RockusbError::UsbError(TransferError::Disconnected))
}

fn usb_err(what: &str, e: RockusbError<TransferError>) -> String {
    if is_disconnect(&e) {
        format!("{TAG_ROCKUSB_DISCONNECTED} board disconnected while trying to {what}")
    } else {
        format!("{TAG_ROCKUSB_ERROR} failed to {what}: {e}")
    }
}

/// Tag an open failure by cause; the frontend turns each into its per-OS remedy.
fn open_err(e: DeviceUnavailable) -> String {
    match &e {
        DeviceUnavailable::UsbError(usb) => match usb.kind() {
            ErrorKind::PermissionDenied => format!("{TAG_ROCKUSB_PERMISSION_DENIED} {e}"),
            ErrorKind::Unsupported => format!("{TAG_ROCKUSB_DRIVER_MISSING} {e}"),
            ErrorKind::Disconnected => format!("{TAG_ROCKUSB_DISCONNECTED} {e}"),
            _ => format!("{TAG_ROCKUSB_CONNECTION_FAILED} {e}"),
        },
        DeviceUnavailable::NotFound => {
            format!("{TAG_ROCKUSB_CONNECTION_FAILED} the device has no rockusb bulk interface")
        }
    }
}

async fn open(info: &DeviceInfo) -> Result<Device, String> {
    with_timeout(Device::from_usb_device_info(info.clone()))
        .await?
        .map_err(open_err)
}

/// A freshly enumerated device can refuse the first open for a moment; driver problems are final.
async fn open_with_retry(info: &DeviceInfo) -> Result<Device, String> {
    let mut last = String::new();
    for attempt in 0..3 {
        match open(info).await {
            Ok(device) => return Ok(device),
            Err(e)
                if e.starts_with(TAG_ROCKUSB_PERMISSION_DENIED)
                    || e.starts_with(TAG_ROCKUSB_DRIVER_MISSING) =>
            {
                return Err(e)
            }
            Err(e) => {
                log_warn!(MODULE, "Open attempt {} failed: {}", attempt + 1, e);
                last = e;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Err(last)
}

async fn upload_loader(
    info: &DeviceInfo,
    soc: &RockchipSoc,
    entries: &[loader::LoaderEntry],
) -> Result<(), String> {
    let mut device = open(info).await?;
    for entry in entries {
        log_info!(
            MODULE,
            "{}: uploading {:?} ({} bytes) to area {:#x}",
            soc.name,
            entry.name,
            entry.data.len(),
            entry.area
        );
        with_timeout(device.write_maskrom_area(entry.area, &entry.data))
            .await?
            .map_err(|e| {
                if is_disconnect(&e) {
                    usb_err("upload the loader", e)
                } else {
                    format!(
                        "{TAG_ROCKUSB_LOADER_FAILED} {} upload failed: {e}",
                        entry.name
                    )
                }
            })?;
        if entry.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(entry.delay_ms as u64)).await;
        }
    }
    // rkdeveloptool waits a second after the last stage before talking to usbplug.
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

/// Wait for the board on the same port to come back running usbplug.
async fn wait_for_loader_mode(path: &str, state: &FlashState) -> Result<DeviceInfo, String> {
    let deadline = Instant::now() + Duration::from_secs(cfg::REENUMERATE_TIMEOUT_SECS);
    loop {
        state.ensure_not_cancelled()?;
        if let Ok((info, found)) = detect::resolve(path) {
            if found.mode == RockchipMode::Loader {
                log_info!(
                    MODULE,
                    "Board is back in Loader mode (PID {:#06x})",
                    found.product_id
                );
                return Ok(info);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{TAG_ROCKUSB_NO_LOADER_MODE} the board didn't come back in Loader mode within {}s",
                cfg::REENUMERATE_TIMEOUT_SECS
            ));
        }
        tokio::time::sleep(Duration::from_millis(cfg::REENUMERATE_POLL_MS)).await;
    }
}

/// Switch to `storage` and read it back: the loader reports no error for storage the board lacks.
async fn select_storage(device: &mut Device, storage: BoardStorage) -> Result<(), String> {
    let wanted = storage.index();
    let can_switch = match with_timeout(device.capability()).await? {
        Ok(cap) => cap.switch_storage(),
        Err(e) if is_disconnect(&e) => return Err(usb_err("read the loader capabilities", e)),
        Err(e) => {
            log_warn!(MODULE, "Capability query failed: {}", e);
            false
        }
    };
    if can_switch {
        with_timeout(device.switch_storage(wanted))
            .await?
            .map_err(|e| usb_err("switch storage", e))?;
    }
    let current = with_timeout(device.storage()).await?;
    match (current, can_switch) {
        (Ok(current), _) if current == wanted => Ok(()),
        (Ok(current), _) => Err(format!(
            "{TAG_ROCKUSB_STORAGE_UNAVAILABLE} {storage:?} not available (board is on {current})"
        )),
        // Older loaders know neither command and always sit on eMMC.
        (Err(e), false) if storage == BoardStorage::Emmc && !is_disconnect(&e) => {
            log_warn!(MODULE, "Storage query unsupported ({}); assuming eMMC", e);
            Ok(())
        }
        (Err(e), _) if is_disconnect(&e) => Err(usb_err("read the selected storage", e)),
        (Err(e), _) => Err(format!(
            "{TAG_ROCKUSB_STORAGE_UNAVAILABLE} this loader can't select {storage:?}: {e}"
        )),
    }
}

const CHUNK_BYTES: usize = cfg::RW_SECTORS as usize * SECTOR_SIZE as usize;

/// Read the next image chunk, zero-padded to whole sectors; `len` is the image bytes it holds.
fn read_chunk(image: &mut File, buf: &mut Vec<u8>, remaining: u64) -> Result<usize, String> {
    let len = (remaining as usize).min(CHUNK_BYTES);
    let padded = len.div_ceil(SECTOR_SIZE as usize) * SECTOR_SIZE as usize;
    buf.clear();
    buf.resize(padded, 0);
    image.read_exact(&mut buf[..len]).map_err(|e| {
        tagged(
            crate::utils::TAG_IMAGE_INVALID,
            format!("image read failed: {e}"),
        )
    })?;
    Ok(len)
}

async fn write_image(
    device: &mut Device,
    image: &mut File,
    image_len: u64,
    state: &FlashState,
) -> Result<(), String> {
    let mut buf = Vec::with_capacity(CHUNK_BYTES);
    let mut offset = 0u64;
    while offset < image_len {
        if state.is_cancelled.load(Ordering::SeqCst) {
            return Err(cancelled_err());
        }
        let len = read_chunk(image, &mut buf, image_len - offset)?;
        let sector = sector_of(offset)?;
        with_timeout(device.write_lba(sector, &buf))
            .await
            .map_err(|e| write_failed_err(offset, e))?
            .map_err(|e| {
                if is_disconnect(&e) {
                    usb_err("write the image", e)
                } else {
                    write_failed_err(offset, e)
                }
            })?;
        offset += len as u64;
        state.written_bytes.store(offset, Ordering::SeqCst);
    }
    log_info!(MODULE, "Wrote {} bytes", offset);
    Ok(())
}

async fn verify_image(
    device: &mut Device,
    image: &mut File,
    image_len: u64,
    state: &FlashState,
) -> Result<(), String> {
    image.seek(SeekFrom::Start(0)).map_err(|e| {
        tagged(
            TAG_VERIFY_READ_FAILED,
            format!("Failed to rewind image for verification: {e}"),
        )
    })?;
    let mut expected = Vec::with_capacity(CHUNK_BYTES);
    let mut actual = vec![0u8; CHUNK_BYTES];
    let mut offset = 0u64;
    while offset < image_len {
        if state.is_cancelled.load(Ordering::SeqCst) {
            return Err(verify_cancelled_err());
        }
        let len = read_chunk(image, &mut expected, image_len - offset)?;
        let readback = &mut actual[..expected.len()];
        let read = with_timeout(device.read_lba(sector_of(offset)?, readback))
            .await?
            .map_err(|e| {
                if is_disconnect(&e) {
                    usb_err("read back the image", e)
                } else {
                    tagged(
                        TAG_VERIFY_READ_FAILED,
                        format!("read-back at byte {offset} failed: {e}"),
                    )
                }
            })?;
        if (read as usize) < len {
            return Err(short_read_err(offset, len, read as usize));
        }
        if let Some(i) = first_difference(&expected[..len], &readback[..len]) {
            return Err(data_mismatch_err(offset + i as u64));
        }
        offset += len as u64;
        state.verified_bytes.store(offset, Ordering::SeqCst);
    }
    check_verified_length(offset, image_len)
}

/// rockusb addresses 32-bit LBAs, so a board can take up to 2 TiB.
fn sector_of(offset: u64) -> Result<u32, String> {
    u32::try_from(offset / SECTOR_SIZE)
        .map_err(|_| write_failed_err(offset, "beyond the 2 TiB LBA limit"))
}

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x != y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn chunks_are_padded_to_whole_sectors() {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&vec![7u8; CHUNK_BYTES + 700]).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let total = (CHUNK_BYTES + 700) as u64;

        let mut buf = Vec::new();
        assert_eq!(read_chunk(&mut f, &mut buf, total).unwrap(), CHUNK_BYTES);
        assert_eq!(buf.len(), CHUNK_BYTES);

        assert_eq!(read_chunk(&mut f, &mut buf, 700).unwrap(), 700);
        assert_eq!(buf.len(), 1024);
        assert!(buf[..700].iter().all(|b| *b == 7));
        assert!(buf[700..].iter().all(|b| *b == 0));
    }

    #[test]
    fn a_short_image_is_an_image_error() {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&[1, 2, 3]).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = Vec::new();
        assert!(read_chunk(&mut f, &mut buf, 10)
            .unwrap_err()
            .starts_with(crate::utils::TAG_IMAGE_INVALID));
    }

    #[test]
    fn sectors_stop_at_the_32_bit_limit() {
        assert_eq!(sector_of(0).unwrap(), 0);
        assert_eq!(sector_of(1024).unwrap(), 2);
        assert_eq!(sector_of(u32::MAX as u64 * 512).unwrap(), u32::MAX);
        assert!(sector_of((u32::MAX as u64 + 1) * 512).is_err());
    }

    #[test]
    fn first_difference_finds_the_byte() {
        assert_eq!(first_difference(b"abcd", b"abcd"), None);
        assert_eq!(first_difference(b"abcd", b"abXd"), Some(2));
    }

    #[test]
    fn a_different_family_is_refused_and_unknown_passes() {
        let rk3588 = socs::find_by_maskrom_pid(0x350b).unwrap();
        let rk3568 = socs::find_by_maskrom_pid(0x350a).unwrap();
        assert!(check_same_family(Some(rk3588), rk3588).is_ok());
        assert!(check_same_family(None, rk3588).is_ok());
        assert!(check_same_family(Some(rk3568), rk3588)
            .unwrap_err()
            .starts_with(TAG_ROCKUSB_SOC_MISMATCH));
    }

    #[test]
    fn transport_errors_map_to_tags() {
        assert!(
            usb_err("x", RockusbError::UsbError(TransferError::Disconnected))
                .starts_with(TAG_ROCKUSB_DISCONNECTED)
        );
        assert!(usb_err("x", RockusbError::UsbError(TransferError::Stall))
            .starts_with(TAG_ROCKUSB_ERROR));
    }
}
