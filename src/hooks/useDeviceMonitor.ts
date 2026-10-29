// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (c) 2025-2026 Daniele Briguglio, superkali@armbian.com

import { useEffect, useCallback } from 'react';
import { isTargetConnected } from './useTauri';
import { POLLING } from '../config';
import type { BlockDevice, TargetBus } from '../types';

/** Clears the selected device once it disconnects; `bus` follows the flash method, never the path. */
export function useDeviceMonitor(
  selectedDevice: BlockDevice | null,
  bus: TargetBus,
  onDeviceDisconnected: () => void,
  enabled: boolean = true
) {
  const checkDevice = useCallback(async () => {
    if (!selectedDevice) return;

    try {
      if (!(await isTargetConnected(selectedDevice, bus))) {
        onDeviceDisconnected();
      }
    } catch {
      // Silently ignore polling errors
    }
  }, [selectedDevice, bus, onDeviceDisconnected]);

  useEffect(() => {
    if (!enabled || !selectedDevice) return;

    checkDevice();

    const interval = setInterval(checkDevice, POLLING.DEVICE_CHECK);
    return () => clearInterval(interval);
  }, [enabled, selectedDevice, checkDevice]);
}
