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
    detect, largest_storage, loader, BoardStorage, RockchipMode, StorageChoice, StorageInfo,
    STAGE_COMPLETE, STAGE_CONNECTING, STAGE_LOADER, STAGE_RECONNECTING, STAGE_RESETTING,
    STAGE_STORAGE, STAGE_VERIFYING, STAGE_WRITING, TAG_ROCKUSB_CONNECTION_FAILED,
    TAG_ROCKUSB_DISCONNECTED, TAG_ROCKUSB_DRIVER_MISSING, TAG_ROCKUSB_ERROR,
    TAG_ROCKUSB_LOADER_FAILED, TAG_ROCKUSB_NO_LOADER_MODE, TAG_ROCKUSB_PERMISSION_DENIED,
    TAG_ROCKUSB_SOC_MISMATCH, TAG_ROCKUSB_STORAGE_UNAVAILABLE, TAG_ROCKUSB_TIMEOUT,
    TAG_ROCKUSB_UNSUPPORTED_SOC,
};
use crate::config::rockusb as cfg;
use crate::flash::verify::{
    check_verified_length, data_mismatch_err, short_read_err, verify_cancelled_err,
    TAG_VERIFY_READ_FAILED,
};
use crate::flash::{cancelled_err, check_capacity, open_image, write_failed_err, FlashState};
use crate::utils::tagged;
use crate::{log_error, log_info, log_warn};

const MODULE: &str = "rockchip::flash";

/// What the user asked for, beyond the image and device.
pub struct BoardFlashRequest<'a> {
    pub image_path: &'a Path,
    pub device_path: &'a str,
    pub storage: StorageChoice,
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
    let _usb = usb_session().await;

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

    // bcdUSB only hints at the mode: RK3576 usbplug keeps the boot ROM's PID and bcdUSB 0x0200,
    // so a board that already runs a loader is told apart by asking it.
    let running_loader = found.mode == RockchipMode::Loader || answers_rockusb(&info).await;
    let info = if running_loader {
        log_info!(
            MODULE,
            "Board already runs a loader; skipping the Maskrom upload"
        );
        info
    } else {
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
        wait_for_loader(req.device_path, Some(&state)).await?
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
    let storage = match req.storage.fixed() {
        Some(storage) => storage,
        None => {
            let found = probe_storages(&mut device).await?;
            let largest = largest_storage(&found).ok_or_else(|| {
                format!(
                    "{TAG_ROCKUSB_STORAGE_UNAVAILABLE} the loader found no storage on the board"
                )
            })?;
            log_info!(MODULE, "Largest storage: {:?} out of {:?}", largest, found);
            largest
        }
    };
    select_storage(&mut device, storage).await?;

    let flash_info = with_timeout(device.flash_info())
        .await?
        .map_err(|e| usb_err("read the storage size", e))?;
    let capacity = flash_info.size();
    log_info!(
        MODULE,
        "{:?} selected: {} sectors ({} bytes)",
        storage,
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
        if let Err(e) = verify_image(&mut device, &mut image, image_len, &state).await {
            // A cancelled read-back still leaves the whole image written: boot it, as a finished flash would.
            if e == verify_cancelled_err() {
                reset_board(&mut device).await;
            }
            return Err(e);
        }
    }

    set_stage(&state, STAGE_RESETTING);
    reset_board(&mut device).await;
    set_stage(&state, STAGE_COMPLETE);
    log_info!(MODULE, "Board flash completed");
    Ok(())
}

/// Reboot the board into the image just written; a board that ignores it only needs a power cycle.
async fn reset_board(device: &mut Device) {
    match with_timeout(device.reset_device(ResetOpcode::Reset)).await {
        Ok(Ok(())) => log_info!(MODULE, "Board reset"),
        Ok(Err(e)) => log_warn!(MODULE, "Board reset failed (non-fatal): {}", e),
        Err(e) => log_warn!(MODULE, "Board reset failed (non-fatal): {}", e),
    }
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

/// True when the device answers a rockusb bulk command, as usbplug and U-Boot rockusb do and the
/// boot ROM doesn't. The probe gets its own handle, so a dropped transfer can't linger into the flash.
async fn answers_rockusb(info: &DeviceInfo) -> bool {
    let Ok(mut device) = open(info).await else {
        return false;
    };
    let probe = Duration::from_millis(cfg::PROBE_TIMEOUT_MS);
    match tokio::time::timeout(probe, device.flash_info()).await {
        Ok(Ok(flash)) => {
            log_info!(
                MODULE,
                "Board answers rockusb ({} sectors on its current storage)",
                flash.sectors()
            );
            true
        }
        Ok(Err(e)) => {
            log_info!(MODULE, "Board doesn't answer rockusb: {}", e);
            false
        }
        Err(_) => {
            log_info!(MODULE, "Board doesn't answer rockusb within {:?}", probe);
            false
        }
    }
}

/// Wait for the board on the same port to come back running usbplug, whatever its descriptor says.
async fn wait_for_loader(path: &str, state: Option<&FlashState>) -> Result<DeviceInfo, String> {
    let deadline = Instant::now() + Duration::from_secs(cfg::REENUMERATE_TIMEOUT_SECS);
    loop {
        if let Some(state) = state {
            state.ensure_not_cancelled()?;
        }
        if let Ok((info, found)) = detect::resolve(path) {
            if answers_rockusb(&info).await {
                log_info!(
                    MODULE,
                    "Board is back running its loader (PID {:#06x}, bcdUSB says {:?})",
                    found.product_id,
                    found.mode
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

// One USB conversation with a board at a time: a storage probe never interleaves with a flash.
static USB_SESSION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn usb_session() -> tokio::sync::MutexGuard<'static, ()> {
    USB_SESSION.lock().await
}

/// Storages a board's running loader can reach, with sizes; None while the board is in the boot ROM.
/// A board in the boot ROM first gets its loader, as the flash would, so the storages are known
/// when the board is picked; the flash then finds the loader running and skips the upload. None
/// for a boot ROM whose SoC this build has no loader for.
pub async fn probe_board_storages(path: &str) -> Result<Option<Vec<StorageInfo>>, String> {
    let _usb = usb_session().await;
    let (info, found) = detect::resolve(path)?;
    let info = if found.mode == RockchipMode::Loader || answers_rockusb(&info).await {
        info
    } else {
        let Some(soc) = socs::find_by_maskrom_pid(found.product_id) else {
            return Ok(None);
        };
        let entries = loader::ensure_loader(soc).await?;
        upload_loader(&info, soc, &entries).await?;
        wait_for_loader(path, None).await?
    };
    let mut device = open_with_retry(&info).await?;
    let found = probe_storages(&mut device).await?;
    log_info!(MODULE, "Storages on {:?}: {:?}", path, found);
    Ok(Some(found))
}

/// The storages a running loader reaches, with sizes. The one it already sits on is read first,
/// with no switch. The others are tried in turn and the board put back, except from UFS/SATA:
/// RK3576 usbplug can't switch back to SCSI, so a board starting there is never moved.
async fn probe_storages(device: &mut Device) -> Result<Vec<StorageInfo>, String> {
    let can_switch = loader_can_switch(device).await?;
    let original = match with_timeout(device.storage()).await? {
        Ok(current) => Some(current),
        Err(e) if is_disconnect(&e) => return Err(usb_err("read the selected storage", e)),
        Err(e) => {
            log_warn!(MODULE, "Storage query failed: {}", e);
            None
        }
    };
    // A loader that can't say where it sits is an old one on eMMC.
    let start = match original {
        Some(current) => BoardStorage::ALL
            .into_iter()
            .find(|s| s.is_reported_as(current)),
        None => Some(BoardStorage::Emmc),
    };

    let mut found = Vec::new();
    if let Some(storage) = start {
        push_size(device, storage, &mut found).await?;
    }
    // Leaving SCSI is one-way on RK3576 usbplug: the switch back rescans SCSI and fails, which
    // strands the board on another storage. Off a SCSI start, only the starting storage is sized.
    if !can_switch || start == Some(BoardStorage::Ufs) {
        return Ok(found);
    }

    for storage in BoardStorage::ALL.into_iter().filter(|s| Some(*s) != start) {
        match with_timeout(device.switch_storage(storage.index())).await? {
            Err(e) if is_disconnect(&e) => return Err(usb_err("switch storage", e)),
            // A storage the board lacks may fail here or only on the readback below.
            _ => {}
        }
        match with_timeout(device.storage()).await? {
            Ok(current) if storage.is_reported_as(current) => {
                push_size(device, storage, &mut found).await?
            }
            Ok(_) => {}
            Err(e) if is_disconnect(&e) => return Err(usb_err("read the selected storage", e)),
            Err(_) => {}
        }
    }

    // Leave the board where it was, so flashing that storage needs no switch.
    if let Some(original) = original {
        if matches!(with_timeout(device.storage()).await?, Ok(current) if current == original) {
            found.sort_by_key(|f| BoardStorage::ALL.iter().position(|s| *s == f.storage));
            return Ok(found);
        }
        let _ = with_timeout(device.switch_storage(original)).await?;
        match with_timeout(device.storage()).await? {
            Ok(current) if current == original => {}
            Ok(current) => log_warn!(
                MODULE,
                "Couldn't switch back to {original} after probing; the loader is on {current}"
            ),
            Err(e) => log_warn!(
                MODULE,
                "Couldn't confirm the switch back to {original}: {e}"
            ),
        }
    }

    found.sort_by_key(|f| BoardStorage::ALL.iter().position(|s| *s == f.storage));
    Ok(found)
}

/// Record the size of the storage the loader has selected, if it reports one.
async fn push_size(
    device: &mut Device,
    storage: BoardStorage,
    found: &mut Vec<StorageInfo>,
) -> Result<(), String> {
    match with_timeout(device.flash_info()).await? {
        Ok(flash) if flash.size() > 0 => found.push(StorageInfo {
            storage,
            size: flash.size(),
        }),
        Ok(_) => log_warn!(MODULE, "{:?} reports no size", storage),
        Err(e) if is_disconnect(&e) => return Err(usb_err("read the storage size", e)),
        Err(e) => log_warn!(MODULE, "No size for {:?}: {}", storage, e),
    }
    Ok(())
}

async fn loader_can_switch(device: &mut Device) -> Result<bool, String> {
    match with_timeout(device.capability()).await? {
        Ok(cap) => Ok(cap.switch_storage()),
        Err(e) if is_disconnect(&e) => Err(usb_err("read the loader capabilities", e)),
        Err(e) => {
            log_warn!(MODULE, "Capability query failed: {}", e);
            Ok(false)
        }
    }
}

/// Switch to `storage` and read it back: the loader reports no error for storage the board lacks.
async fn select_storage(device: &mut Device, storage: BoardStorage) -> Result<(), String> {
    let wanted = storage.index();
    // Already there: no switch, since a switch to SATA/UFS rescans SCSI even when it is current.
    if let Ok(Ok(current)) = with_timeout(device.storage()).await {
        if storage.is_reported_as(current) {
            log_info!(
                MODULE,
                "{storage:?} already selected (the loader reports {current})"
            );
            return Ok(());
        }
    }
    let can_switch = loader_can_switch(device).await?;
    if can_switch {
        with_timeout(device.switch_storage(wanted))
            .await?
            .map_err(|e| usb_err("switch storage", e))?;
    }
    let current = with_timeout(device.storage()).await?;
    match (current, can_switch) {
        (Ok(current), _) if storage.is_reported_as(current) => {
            if current != wanted {
                log_info!(
                    MODULE,
                    "{storage:?} selected (the loader reports it as {current})"
                );
            }
            Ok(())
        }
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
            let mismatch = readback[..len].to_vec();
            diagnose_mismatch(device, image, offset, i, &expected[..len], &mismatch).await;
            return Err(data_mismatch_err(offset + i as u64));
        }
        offset += len as u64;
        state.verified_bytes.store(offset, Ordering::SeqCst);
    }
    check_verified_length(offset, image_len)
}

/// Log what the board returned for a chunk that failed verification, so a bad write, a flaky read
/// and an address wrap can be told apart from one log.
async fn diagnose_mismatch(
    device: &mut Device,
    image: &mut File,
    offset: u64,
    first: usize,
    expected: &[u8],
    actual: &[u8],
) {
    let at = first & !15;
    let window = |b: &[u8]| hex::encode(&b[at..(at + 32).min(b.len())]);
    let differing = expected.iter().zip(actual).filter(|(e, a)| e != a).count();
    log_error!(
        MODULE,
        "Mismatch in chunk at byte {offset} (sector {}): first differing byte +{first}, {differing} of {} bytes differ",
        offset / SECTOR_SIZE,
        expected.len()
    );
    log_error!(MODULE, "  expected +{at}: {}", window(expected));
    log_error!(MODULE, "  actual   +{at}: {}", window(actual));
    let fill = if actual.iter().all(|b| *b == 0) {
        "all zeros"
    } else if actual.iter().all(|b| *b == 0xff) {
        "all 0xff"
    } else {
        "data"
    };
    log_error!(MODULE, "  board returned {fill}");

    // A second read: equal means the board holds this data, different means the read path is flaky.
    let mut again = vec![0u8; actual.len()];
    if let Ok(sector) = sector_of(offset) {
        match with_timeout(device.read_lba(sector, &mut again)).await {
            Ok(Ok(_)) => log_error!(
                MODULE,
                "  re-read: {}",
                if again == actual {
                    "same as the first read"
                } else if again == expected {
                    "now matches the image (flaky read)"
                } else {
                    "different from both (flaky read)"
                }
            ),
            other => log_error!(MODULE, "  re-read failed: {:?}", other.err()),
        }
    }

    // Does the returned chunk equal the image some power-of-two distance lower (an address wrap)?
    for shift in [32u64 << 20, 16 << 20, 8 << 20, 4 << 20] {
        let Some(src) = offset.checked_sub(shift) else {
            continue;
        };
        let mut candidate = vec![0u8; actual.len()];
        if image.seek(SeekFrom::Start(src)).is_ok()
            && image.read_exact(&mut candidate).is_ok()
            && candidate == actual
        {
            log_error!(
                MODULE,
                "  returned data is the image at byte {src} ({} MiB lower)",
                shift >> 20
            );
        }
    }

    // Reads of other sizes and places around the failure: is the cut exact at this sector, and is
    // everything beyond it unreadable, or only what 64 KiB reads return?
    let first_bad = (offset + first as u64) / SECTOR_SIZE;
    let probes: [(&str, u64, usize); 6] = [
        ("1 sector at the failure", first_bad, 1),
        ("one 4 KiB block at the failure", first_bad & !7, 8),
        (
            "16 sectors across the failure",
            first_bad.saturating_sub(8),
            16,
        ),
        ("64 KiB at +1 MiB", first_bad + 2048, 128),
        ("64 KiB at +256 MiB", first_bad + (256 << 11), 128),
        (
            "64 KiB at the last good chunk",
            first_bad.saturating_sub(128),
            128,
        ),
    ];
    for (label, sector, count) in probes {
        let mut board = vec![0u8; count * SECTOR_SIZE as usize];
        let Ok(lba) = u32::try_from(sector) else {
            continue;
        };
        let read = with_timeout(device.read_lba(lba, &mut board)).await;
        let mut want = vec![0u8; board.len()];
        let have_image = image.seek(SeekFrom::Start(sector * SECTOR_SIZE)).is_ok()
            && image.read_exact(&mut want).is_ok();
        let verdict = match read {
            Ok(Ok(_)) if have_image && board == want => "matches the image".to_string(),
            Ok(Ok(_)) if board.iter().all(|b| *b == 0xcc) => "all 0xcc".to_string(),
            Ok(Ok(_)) => match board.iter().position(|b| *b == 0xcc) {
                Some(i) if board[i..].iter().all(|b| *b == 0xcc) => {
                    format!(
                        "real data, then 0xcc from +{i} (sector {})",
                        sector + i as u64 / SECTOR_SIZE
                    )
                }
                _ => format!("other data: {}", hex::encode(&board[..16])),
            },
            Ok(Err(e)) => format!("read failed: {e}"),
            Err(e) => e,
        };
        log_error!(MODULE, "  {label} (sector {sector}, {count}): {verdict}");
    }
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
