// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (c) 2026 Daniele Briguglio, superkali@armbian.com

//! Tauri command handlers for Rockchip boards over USB (Maskrom / Loader mode): detection and flashing.

use std::path::PathBuf;
use tauri::State;

use crate::autoconfig::{prepare_flash_copy, AutoconfigConfig, PrepError};
use crate::flash::reject_simulated;
use crate::rockchip::flash::{rockusb_flash, BoardFlashRequest};
use crate::rockchip::{self, BoardStorage, RockchipDevice};
use crate::utils::{autoconfig_temp_dir, ensure_regular_nonempty};
use crate::{log_error, log_info};

use super::state::AppState;

/// Detect Rockchip boards connected over USB in Maskrom or Loader mode; empty list if none.
#[tauri::command]
pub async fn get_rockusb_devices() -> Result<Vec<RockchipDevice>, String> {
    rockchip::detect::get_rockusb_devices()
}

/// Flash a decompressed image to the storage of the board at `device_path` (`rockusb://...` from
/// `get_rockusb_devices`). Pipeline: [profile copy] -> [Maskrom: loader upload, re-enumerate]
/// -> select storage -> LBA write -> read-back verify -> reset.
#[tauri::command]
pub async fn flash_rockusb_image(
    image_path: String,
    device_path: String,
    storage: BoardStorage,
    verify: bool,
    board_soc: Option<String>,
    autoconfig: Option<AutoconfigConfig>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    log_info!(
        "rockusb_operations",
        "Starting board flash: {} -> {} ({:?}, verify: {}, board SoC: {:?}, autoconfig: {})",
        image_path,
        device_path,
        storage,
        verify,
        board_soc,
        autoconfig.is_some()
    );

    let flash_state = state.flash_state.clone();
    let session = flash_state.begin_session().await.map_err(|refusal| {
        log_error!(
            "rockusb_operations",
            "Board flash not started: {}",
            refusal.message()
        );
        refusal.message()
    })?;

    let result = async {
        reject_simulated(&device_path)?;
        // Fail fast on a gone or ambiguous board, before any profile copy or loader download.
        rockchip::detect::resolve(&device_path)?;
        let image_path = PathBuf::from(&image_path);
        ensure_regular_nonempty(&image_path)?;

        // The preset goes into a per-flash copy (removed on drop) so the cached image stays pristine.
        let working_copy = match autoconfig {
            Some(profile) => Some(
                prepare_flash_copy(
                    image_path.clone(),
                    autoconfig_temp_dir(),
                    profile,
                    true,
                    flash_state.clone(),
                )
                .await
                .map_err(PrepError::into_flash_error)?,
            ),
            None => None,
        };
        flash_state.ensure_not_cancelled()?;
        let write_path = working_copy
            .as_ref()
            .map_or(image_path, |copy| copy.path().to_path_buf());

        rockusb_flash(
            BoardFlashRequest {
                image_path: &write_path,
                device_path: &device_path,
                storage,
                verify,
                board_soc: board_soc.as_deref().filter(|s| !s.is_empty()),
            },
            flash_state.clone(),
        )
        .await
    }
    .await;
    drop(session);

    match &result {
        Ok(()) => log_info!("rockusb_operations", "Board flash completed successfully"),
        Err(e) => log_error!("rockusb_operations", "Board flash failed: {}", e),
    }
    result
}
