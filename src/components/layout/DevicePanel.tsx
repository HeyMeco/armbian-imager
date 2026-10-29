// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (c) 2026 Daniele Briguglio, superkali@armbian.com

import { useState, useEffect, useRef, useCallback, useMemo } from 'react';
import {
  RefreshCw, TriangleAlert, Shield, Usb, Lock, Cpu, ArrowRight, ChevronDown, HardDrive,
} from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { PanelError, DeviceIcon, getDeviceBadge, BoardImage, MarqueeText, EmptyState } from '../shared';
import type {
  BlockDevice, AutoconfigProfile, AutoconfigProfilesChangedDetail, FlashMethod, FlashTarget, BoardStorage,
  BoardStorageInfo,
} from '../../types';
import { BOARD_STORAGE, FLASH_TARGET, isBoardMethod, isEdlMethod, largestBoardStorage, targetBus } from '../../types';
import { formatBytes } from '../../utils';
import { SegmentedControl, SelectMenu } from '../settings/controls';
import { getTargetDevices, probeRockusbStorages } from '../../hooks/useTauri';
import { getAutoconfigProfiles, getAllowSystemDevices } from '../../hooks/useSettings';
import { useAsyncData } from '../../hooks/useAsyncData';
import { useErrorRetry } from '../../hooks/useErrorRetry';
import { useSkeletonLoading } from '../../hooks/useSkeletonLoading';
import { POLLING, UI, EVENTS, qdlInstructionsKey } from '../../config';
import { isProfileOfferedFor } from '../../config/autoconfig';
import { ProfileMenu, ProfileWizard } from '../autoconfig';
import { getDeviceColors } from '../../config/deviceColors';
import { getDeviceType, devicesChanged, sortDevices } from '../../utils/deviceUtils';

interface DevicePanelProps {
  /** Pick a device, revealing the confirm summary (does not flash yet). */
  onSelect: (device: BlockDevice) => void;
  /** Confirm the summary and start flashing. */
  onConfirm: () => void;
  /** Cancel the summary, back to the device list. */
  onCancel: () => void;
  /** The picked device; when set, the panel shows the confirm summary. */
  selectedDevice: BlockDevice | null;
  /** Flash method for the selected image and target ("block", "qdl", "qdl-ufs" or "rockusb"). */
  flashMethod?: FlashMethod;
  /** Offer the disk vs Rockchip board switch (block image whose board isn't known to be non-Rockchip). */
  boardFlashAvailable?: boolean;
  flashTarget?: FlashTarget;
  onFlashTargetChange?: (target: FlashTarget) => void;
  /** Storage on the board a board flash writes to, picked in the confirm summary. */
  boardStorage?: BoardStorage;
  onBoardStorageChange?: (storage: BoardStorage) => void;
  /** EDL-entry hint ("button"/"jumper") for the selected board, drives the QDL instructions. */
  edlEntry?: string | null;
  /** Upstream selections (manufacturer/board/OS) shown in the confirm summary. */
  summary?: { label: string; value: string; sub?: string | null }[];
  /** Cached board photo shown at the top of the confirm summary. */
  boardImage?: string | null;
  /** Selected board slug; profiles scoped to other boards are not offered */
  boardSlug?: string | null;
  boardName?: string | null;
  /** Whether autoconfig profiles apply (Armbian images only; hidden for generic custom images). */
  supportsAutoconfig?: boolean;
}

/** Inline storage browser: device list while picking, confirm summary once `selectedDevice` is set. */
export function DevicePanel({
  onSelect,
  onConfirm,
  onCancel,
  selectedDevice,
  flashMethod,
  boardFlashAvailable = false,
  flashTarget = FLASH_TARGET.DISK,
  onFlashTargetChange,
  boardStorage = BOARD_STORAGE.AUTO,
  onBoardStorageChange,
  edlEntry,
  summary = [],
  boardImage,
  boardSlug = null,
  boardName = null,
  supportsAutoconfig = true,
}: DevicePanelProps) {
  const { t } = useTranslation();
  const [showSystemDevices, setShowSystemDevices] = useState(false);
  // Persistent opt-in to show and flash internal/system disks at the user's own risk.
  const [allowSystemDevices, setAllowSystemDevices] = useState(false);

  useEffect(() => {
    const load = () => {
      getAllowSystemDevices().then(setAllowSystemDevices).catch(() => {});
    };
    load();
    window.addEventListener(EVENTS.SETTINGS_CHANGED, load);
    return () => window.removeEventListener(EVENTS.SETTINGS_CHANGED, load);
  }, []);

  const prevDevicesRef = useRef<BlockDevice[] | null>(null);
  const [devices, setDevices] = useState<BlockDevice[]>([]);

  // Both QDL (TAR) and UFS images flash over EDL, so they share device detection (getQdlDevices).
  const isQdlMode = isEdlMethod(flashMethod);
  // A Rockchip board over USB: Maskrom or Loader devices instead of disks.
  const bus = targetBus(flashMethod);
  const isRockusb = bus === 'rockusb';
  // Either kind of board is a USB device, not a storage drive.
  const isUsbBoard = isBoardMethod(flashMethod);

  const targetOptions = useMemo(
    () => [
      { value: FLASH_TARGET.DISK, label: t('device.targetDisk'), icon: <HardDrive /> },
      { value: FLASH_TARGET.BOARD, label: t('device.targetBoard'), icon: <Cpu /> },
    ],
    [t]
  );
  // Storages the board's running loader reported, with sizes; null until then (or while in the boot ROM).
  const [probedStorages, setProbedStorages] = useState<BoardStorageInfo[] | null>(null);
  // Every storage stays pickable: the probe only adds sizes, since a loader may fail to switch to
  // one the board has. "Largest available" is offered until a probe has found something.
  const storageOptions = useMemo(() => {
    const probed = probedStorages?.length ? probedStorages : null;
    return Object.values(BOARD_STORAGE)
      .filter((value) => !(probed && value === BOARD_STORAGE.AUTO))
      .map((value) => {
        const size = probed?.find((s) => s.storage === value)?.size;
        return {
          value,
          label: size ? `${t(`flash.boardStorage.${value}`)} · ${formatBytes(size)}` : t(`flash.boardStorage.${value}`),
          hint: value === BOARD_STORAGE.AUTO ? t('flash.boardStorage.autoHint') : undefined,
        };
      });
  }, [probedStorages, t]);

  // Ask a board that already runs a loader what it has; a boot ROM board keeps "Largest available".
  const probePath = isRockusb ? selectedDevice?.path ?? null : null;
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- sizes belong to the board being confirmed
    setProbedStorages(null);
    if (!probePath) return;
    let alive = true;
    probeRockusbStorages(probePath)
      .then((found) => alive && setProbedStorages(found))
      .catch(() => {
        // The flash resolves "Largest available" itself; the probe only adds sizes.
      });
    return () => {
      alive = false;
    };
  }, [probePath]);

  // Once sizes are known, "Largest available" becomes the largest one found; a manual pick stays.
  useEffect(() => {
    if (!probedStorages?.length || !onBoardStorageChange) return;
    if (boardStorage !== BOARD_STORAGE.AUTO) return;
    const largest = largestBoardStorage(probedStorages);
    if (largest) onBoardStorageChange(largest);
  }, [probedStorages, boardStorage, onBoardStorageChange]);

  // Autoconfig profiles apply to Armbian images (both sd/.img.xz and QDL); hidden for generic custom images.
  const showAutoconfig = supportsAutoconfig;

  // Opt-in autoconfig profile picker (flash-time only, Armbian images).
  const [menuOpen, setMenuOpen] = useState(false);
  const [selectedProfileId, setSelectedProfileId] = useState('');
  const [wizardOpen, setWizardOpen] = useState(false);
  // A profile the wizard just created, picked once the reloaded list has it
  const [pendingPickId, setPendingPickId] = useState<string | null>(null);
  const profileRowRef = useRef<HTMLButtonElement>(null);
  const wizardBoard = useMemo(
    () => (boardSlug && boardName ? { slug: boardSlug, name: boardName } : null),
    [boardSlug, boardName]
  );

  const { data: profilesData, reload: reloadProfiles } = useAsyncData<AutoconfigProfile[]>(
    () => (showAutoconfig ? getAutoconfigProfiles() : Promise.resolve([])),
    [showAutoconfig]
  );
  const profiles = useMemo(
    () => (profilesData ?? []).filter((p) => isProfileOfferedFor(p, boardSlug)),
    [profilesData, boardSlug]
  );
  const selectedProfile = profiles.find((p) => p.id === selectedProfileId) ?? null;

  // Notify App of the opt-in selection; default "" means no profile (unchanged behaviour).
  const handleProfileChange = useCallback((id: string) => {
    setSelectedProfileId(id);
    setMenuOpen(false);
    window.dispatchEvent(
      new CustomEvent(EVENTS.AUTOCONFIG_PROFILE_SELECTED, { detail: { id: id || null } })
    );
  }, []);

  // Keep the picker in sync with profile create/edit/delete done in Settings, without re-mounting.
  useEffect(() => {
    if (!showAutoconfig) return;
    const onProfilesChanged = (e: Event) => {
      const detail = (e as CustomEvent<AutoconfigProfilesChangedDetail>).detail;
      reloadProfiles();
      if (detail?.action === 'deleted' && detail.id === selectedProfileId) {
        handleProfileChange('');
      }
    };
    window.addEventListener(EVENTS.PROFILES_CHANGED, onProfilesChanged);
    return () => window.removeEventListener(EVENTS.PROFILES_CHANGED, onProfilesChanged);
  }, [showAutoconfig, selectedProfileId, reloadProfiles, handleProfileChange]);

  // A profile edited to another board scope is no longer offered here, so it cannot stay picked.
  useEffect(() => {
    if (!profilesData || !selectedProfileId || selectedProfile) return;
    // eslint-disable-next-line react-hooks/set-state-in-effect -- drop a pick the board no longer offers
    handleProfileChange('');
  }, [profilesData, selectedProfileId, selectedProfile, handleProfileChange]);

  useEffect(() => {
    if (!pendingPickId || !profiles.some((p) => p.id === pendingPickId)) return;
    // eslint-disable-next-line react-hooks/set-state-in-effect -- pick the new profile once the reloaded list offers it
    setPendingPickId(null);
    handleProfileChange(pendingPickId);
  }, [pendingPickId, profiles, handleProfileChange]);

  const openWizard = useCallback(() => {
    setMenuOpen(false);
    setWizardOpen(true);
  }, []);

  const { data: rawDevices, loading, error: loadError, reload } = useAsyncData<BlockDevice[]>(
    () => getTargetDevices(bus),
    [bus]
  );
  const { error, retrying, retry, panelRef } = useErrorRetry(loadError, loading, reload);

  // Also "ready" when loading finished with no devices found.
  const devicesReady = useMemo(() => {
    return (devices && devices.length > 0) || !loading;
  }, [devices, loading]);

  // System disks are shown when unlocked via settings, or peeked via the local toggle.
  const showSystem = allowSystemDevices || showSystemDevices;
  const filteredDevices = useMemo(() => {
    const filtered = showSystem ? devices : devices.filter((d) => !d.is_system);
    return sortDevices(filtered);
  }, [devices, showSystem]);

  const { showSkeleton } = useSkeletonLoading(loading, devicesReady);
  // useSkeletonLoading starts false, so the first load needs its own guard; polling never sets `loading`.
  const showDeviceSkeleton = showSkeleton || (loading && rawDevices === null);

  // Sync external device data into local state only when it actually changes.
  useEffect(() => {
    if (!rawDevices) return;
    if (devicesChanged(prevDevicesRef.current, rawDevices)) {
      prevDevicesRef.current = rawDevices;
      // eslint-disable-next-line react-hooks/set-state-in-effect -- Sync external device data for change detection
      setDevices(sortDevices(rawDevices));
    }
  }, [rawDevices]);

  // Poll for device changes (detect new USB/SD insertions) while browsing.
  const pollDevices = useCallback(async () => {
    try {
      const newDevices = await getTargetDevices(bus);
      if (devicesChanged(prevDevicesRef.current, newDevices)) {
        prevDevicesRef.current = newDevices;
        setDevices(sortDevices(newDevices));
      }
    } catch {
      // Ignore transient polling errors
    }
  }, [bus]);

  useEffect(() => {
    if (selectedDevice) return;
    const interval = setInterval(pollDevices, POLLING.DEVICE_CHECK);
    return () => clearInterval(interval);
  }, [selectedDevice, pollDevices]);

  function handleDeviceClick(device: BlockDevice) {
    if (device.is_read_only || (device.is_system && !allowSystemDevices)) return;
    onSelect(device);
  }

  const refreshButton = (
    <button className="device-refresh-btn" onClick={reload} disabled={loading}>
      <RefreshCw size={15} className={loading ? 'spin' : ''} />
      {t('device.refresh')}
    </button>
  );

  // Confirm view: build + target summary, erase warning, and a flash action.
  if (selectedDevice) {
    return (
      <div className="mfr-panel device-panel">
        <div className="device-panel__body device-confirm">
          <div className="device-confirm__inner">
            {boardImage && (
              <div className="device-confirm__board">
                {/* BoardImage owns the logo watermark fallback when no photo exists. */}
                <BoardImage src={boardImage} alt="" />
              </div>
            )}

            <div className="device-confirm__main">
              <h2 className="device-confirm__title">{t('flash.confirmTitle')}</h2>
              <p className="device-confirm__subtitle">{t('flash.confirmText')}</p>

              <ul className="device-summary">
                {summary.map((row) => (
                  <li key={row.label} className="device-summary__row">
                    <span className="device-summary__label">{row.label}</span>
                    <span className="device-summary__valueinfo">
                      {/* Long values (e.g. custom image filenames) auto-scroll instead of truncating. */}
                      <MarqueeText text={row.value} className="device-summary__value" maxWidth={UI.MARQUEE.SUMMARY_VALUE_WIDTH} />
                      {row.sub && <span className="device-summary__sub">{row.sub}</span>}
                    </span>
                  </li>
                ))}
                {/* Target device as a label/value row, consistent with the rows above. */}
                <li className="device-summary__target">
                  <span className="device-summary__label">{t(isUsbBoard ? 'home.device' : 'home.storage')}</span>
                  <span className="device-summary__targetinfo">
                    <MarqueeText
                      text={selectedDevice.model || selectedDevice.name}
                      className="device-summary__targetname"
                      maxWidth={UI.MARQUEE.SUMMARY_TARGET_WIDTH}
                    />
                    <span className="device-summary__targetsub">
                      {selectedDevice.name}
                      {selectedDevice.size_formatted ? ` • ${selectedDevice.size_formatted}` : ''}
                    </span>
                  </span>
                </li>
                {/* Board flash: which of the board's storages gets the image (the loader switches to it). */}
                {isRockusb && onBoardStorageChange && (
                  <li className="device-summary__target device-summary__boardstorage">
                    <span className="device-summary__label">{t('flash.boardStorage.rowLabel')}</span>
                    <SelectMenu
                      value={boardStorage}
                      options={storageOptions}
                      onChange={onBoardStorageChange}
                      ariaLabel={t('flash.boardStorage.rowLabel')}
                    />
                  </li>
                )}
                {/* System/internal target: contextual danger row tied to the storage above. */}
                {selectedDevice.is_system && (
                  <li className="device-summary__syswarn">
                    <TriangleAlert size={15} />
                    <span>{t('flash.systemDeviceWarning')}</span>
                  </li>
                )}
                {/* Opt-in autoconfig profile, integrated as the final summary row (Armbian images only). */}
                {showAutoconfig && (
                  <li className="device-summary__profile">
                    <button
                      ref={profileRowRef}
                      type="button"
                      className={`device-summary__profilerow ${menuOpen ? 'is-open' : ''}`}
                      onClick={() => setMenuOpen((v) => !v)}
                      aria-haspopup="listbox"
                      aria-expanded={menuOpen}
                    >
                      <span className="device-summary__label">{t('flash.profile.rowLabel')}</span>
                      <span className="device-summary__profileval">
                        {selectedProfile ? (
                          <span className="device-summary__profilename">{selectedProfile.name}</span>
                        ) : (
                          <span className="device-summary__profilenone">{t('flash.profile.none')}</span>
                        )}
                        <ChevronDown size={15} className="device-summary__profilechevron" />
                      </span>
                    </button>

                    {menuOpen && (
                      <ProfileMenu
                        anchorRef={profileRowRef}
                        profiles={profiles}
                        selectedId={selectedProfileId}
                        onSelect={handleProfileChange}
                        onNewProfile={openWizard}
                        onClose={() => setMenuOpen(false)}
                      />
                    )}
                    {wizardOpen && (
                      <ProfileWizard
                        board={wizardBoard}
                        boardImage={boardImage ?? null}
                        fromFlash
                        returnFocusRef={profileRowRef}
                        onClose={() => setWizardOpen(false)}
                        onCreated={(profile) => {
                          if (isProfileOfferedFor(profile, boardSlug)) setPendingPickId(profile.id);
                        }}
                      />
                    )}
                  </li>
                )}
              </ul>

              <div className="device-confirm__warn">
                <TriangleAlert size={15} />
                {t('flash.confirmWarning')}
              </div>

              <div className="device-confirm__actions">
                <button type="button" className="device-confirm__cancel" onClick={onCancel}>
                  {t('common.cancel')}
                </button>
                <button type="button" className="device-confirm__go" onClick={onConfirm}>
                  {t('flash.eraseAndFlash')}
                </button>
              </div>
            </div>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="mfr-panel device-panel" ref={panelRef}>
      <div className="device-panel__head">
        {boardFlashAvailable && onFlashTargetChange && (
          <SegmentedControl
            value={flashTarget}
            options={targetOptions}
            onChange={onFlashTargetChange}
            ariaLabel={t('device.targetLabel')}
          />
        )}
        <div className="device-alert">
          <span className="device-alert__icon">
            <TriangleAlert size={16} />
          </span>
          <span className="device-alert__text">{t('flash.dataWarning')}</span>

          {!isUsbBoard && !allowSystemDevices && (
            <button
              type="button"
              onClick={() => setShowSystemDevices(!showSystemDevices)}
              className={`device-toggle ${showSystemDevices ? 'is-active' : ''}`}
            >
              <Shield size={14} />
              <span>{showSystemDevices ? t('device.hideSystemDevices') : t('device.showSystemDevices')}</span>
            </button>
          )}
        </div>
      </div>

      {error ? (
        <PanelError
          error={error}
          onRetry={retry}
          retrying={retrying}
          source={isQdlMode ? 'edl' : isRockusb ? 'board' : 'devices'}
        />
      ) : (
        <div className="device-panel__body">
          {showDeviceSkeleton && (
            <div className="device-grid">
              {Array.from({ length: UI.SKELETON.DEVICE_PANEL }).map((_, i) => (
                <div key={i} className="device-card is-skeleton">
                  <span className="sk-shim dev-sk-icon" />
                  <span className="device-card__info">
                    <span className="sk-shim dev-sk-name" />
                    <span className="sk-shim dev-sk-sub" />
                  </span>
                </div>
              ))}
            </div>
          )}

          {filteredDevices.length === 0 && !showSkeleton && !loading && (
            <EmptyState
              icon={isUsbBoard ? Cpu : Usb}
              title={
                isQdlMode ? t('device.qdlNotFound') : isRockusb ? t('device.rockusbNotFound') : t('modal.noDevices')
              }
              hint={
                isQdlMode
                  ? t(qdlInstructionsKey(edlEntry))
                  : isRockusb
                    ? t('device.rockusbInstructions')
                    : t('modal.insertDevice')
              }
              action={{ label: t('device.refresh'), icon: RefreshCw, onClick: reload }}
            />
          )}

          {!showDeviceSkeleton && filteredDevices.length > 0 && (
            <>
              <div className="device-grid">
                {filteredDevices.map((device) => {
                  const deviceType = getDeviceType(device);
                  const badge = getDeviceBadge(deviceType, t);
                  const colors = getDeviceColors(deviceType);
                  const isDisabled = device.is_read_only || (device.is_system && !allowSystemDevices);
                  // Unlocked system disk: show its real drive glyph, not the lock (it is selectable).
                  const iconType =
                    device.is_system && allowSystemDevices
                      ? getDeviceType({ ...device, is_system: false })
                      : deviceType;

                  return (
                    <button
                      key={device.path}
                      type="button"
                      className={`device-card ${isDisabled ? 'is-disabled' : ''}`}
                      onClick={() => handleDeviceClick(device)}
                      disabled={isDisabled}
                    >
                      <span
                        className="device-card__icon"
                        style={{ backgroundColor: colors.background, color: colors.text }}
                      >
                        <DeviceIcon type={iconType} size={22} />
                      </span>
                      <span className="device-card__info">
                        <span className="device-card__name">
                          {device.model || device.name}
                          {badge && <span className={`${deviceType}-badge`}>{badge}</span>}
                        </span>
                        <span className="device-card__sub">
                          {device.name}
                          {device.size_formatted ? ` • ${device.size_formatted}` : ''}
                        </span>
                      </span>
                      {device.is_read_only ? (
                        <span className="device-card__lock">
                          <Lock size={11} />
                          {t('device.locked')}
                        </span>
                      ) : (
                        !isDisabled && <ArrowRight className="device-card__arrow" size={18} />
                      )}
                    </button>
                  );
                })}
              </div>
              <div className="device-refresh">{refreshButton}</div>
            </>
          )}
        </div>
      )}
    </div>
  );
}
