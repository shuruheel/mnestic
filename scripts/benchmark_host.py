"""Read-only host admission and contamination accounting for local benchmarks."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

import timing_harness as harness

NATIVE = {'cargo', 'rustc', 'clang', 'clang++', 'clippy-driver', 'cc', 'c++',
          'rust-lld', 'ld', 'maturin', 'keyed_txtime_timing'}


def power():
    if sys.platform != 'darwin':
        raise RuntimeError('phase 3 host protocol requires the declared macOS host')
    battery = subprocess.check_output(['pmset', '-g', 'batt'], text=True)
    settings = subprocess.check_output(['pmset', '-g', 'custom'], text=True)
    modes = re.findall(r'^\s*lowpowermode\s+(\d+)\s*$', settings, re.M)
    return dict(ac_power="Now drawing from 'AC Power'" in battery,
                low_power_disabled=bool(modes) and all(m == '0' for m in modes),
                battery=battery, settings=settings)


def swapouts(raw):
    match = re.search(r'^Swapouts:\s+(\d+)\.?\s*$', raw, re.M)
    if match is None:
        raise ValueError('missing vm_stat Swapouts counter')
    return int(match[1])


def observation(table, excluded, swap_raw, disk, power_state):
    other = [(pid, cpu, name) for pid, _, _, cpu, name in table if pid not in excluded]
    native = [(pid, name) for pid, _, name in other if Path(name).name in NATIVE]
    busy = [(pid, cpu, name) for pid, cpu, name in other if cpu > 25]
    stat = os.statvfs(disk)
    return dict(monotonic_s=time.monotonic(), unix_s=time.time(),
                free_mib=stat.f_bavail*stat.f_frsize/1024**2,
                native=native, busy=busy, swapouts=swapouts(swap_raw), **power_state)


def invalid(sample, previous_swap):
    if sample['free_mib'] < 2048:
        return 'disk_limit'
    if not sample['ac_power'] or not sample['low_power_disabled']:
        return 'power_condition'
    if sample['native']:
        return 'competing_native_process'
    if previous_swap is not None and sample['swapouts'] != previous_swap:
        return 'swapout_change'
    return None


def preflight(path, disk, max_seconds=300, quiet_seconds=30):
    """Retain every sample, including ineligible ones. Return one terminal result."""
    started = time.monotonic()
    quiet_start = None
    previous = None
    last_reason = 'no_observation'
    with Path(path).open('x') as out:
        while time.monotonic()-started < max_seconds:
            _, table, ids = harness.processes(os.getpid())
            sample = observation(table, ids, subprocess.check_output(['vm_stat'], text=True), disk, power())
            reason = invalid(sample, previous)
            previous = sample['swapouts']
            if sample['busy'] and reason is None:
                reason = 'competing_cpu'
            last_reason = reason
            quiet_start = None if reason else (quiet_start if quiet_start is not None else time.monotonic())
            sample.update(reason=reason, quiet_elapsed_s=0 if quiet_start is None else time.monotonic()-quiet_start)
            out.write(json.dumps(sample)+'\n'); out.flush()
            if sample['quiet_elapsed_s'] >= quiet_seconds:
                return dict(eligible=True, elapsed_s=time.monotonic()-started, final_sample=sample)
            time.sleep(min(1, max(0, max_seconds-(time.monotonic()-started))))
    return dict(eligible=False, reason=last_reason or 'insufficient_quiet_window', elapsed_s=time.monotonic()-started)


class Monitor:
    """Called by the supervisor before interpreting each one-second resource sample."""
    def __init__(self, path, disk, initial_swapouts):
        self.out = Path(path).open('x')
        self.disk = disk
        self.previous = initial_swapouts
        self.busy_streaks = {}

    def observe(self, table, ids, swap_raw):
        sample = observation(table, ids | {os.getpid()}, swap_raw, self.disk, power())
        reason = invalid(sample, self.previous)
        self.previous = sample['swapouts']
        self.busy_streaks = {pid: self.busy_streaks.get(pid, 0)+1 for pid, _, _ in sample['busy']}
        if reason is None and any(n >= 5 for n in self.busy_streaks.values()):
            reason = 'sustained_competing_cpu'
        sample.update(reason=reason, busy_streaks=self.busy_streaks)
        self.out.write(json.dumps(sample)+'\n'); self.out.flush()
        return reason

    def close(self):
        self.out.close()
