"""Read-only host admission and contamination accounting for local and Linux Fly benchmarks."""
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import time

import timing_harness as harness

NATIVE = {'cargo', 'rustc', 'clang', 'clang++', 'clippy-driver', 'cc', 'c++',
          'rust-lld', 'ld', 'maturin', 'keyed_txtime_timing', 'keyed_txtime_ti'}


def power():
    if sys.platform == 'darwin':
        battery = subprocess.check_output(['pmset', '-g', 'batt'], text=True)
        settings = subprocess.check_output(['pmset', '-g', 'custom'], text=True)
        modes = re.findall(r'^\s*lowpowermode\s+(\d+)\s*$', settings, re.M)
        return dict(platform='darwin', ac_power="Now drawing from 'AC Power'" in battery,
                    low_power_disabled=bool(modes) and all(m == '0' for m in modes),
                    battery=battery, settings=settings)
    if sys.platform == 'linux':
        return dict(platform='linux', host_class='linux_dedicated',
                    ac_power=True, low_power_disabled=True,
                    kernel=platform.release(), machine=platform.machine())
    raise RuntimeError(f'unsupported benchmark host platform: {sys.platform}')


def read_vmstat():
    if sys.platform == 'darwin':
        return subprocess.check_output(['vm_stat'], text=True)
    if sys.platform == 'linux':
        return Path('/proc/vmstat').read_text()
    raise RuntimeError(f'unsupported benchmark host platform: {sys.platform}')


def read_linux_steal():
    if sys.platform != 'linux':
        return None
    try:
        for line in Path('/proc/stat').read_text().splitlines():
            if line.startswith('cpu '):
                parts = line.split()
                # cpu user nice system idle iowait irq softirq steal ...
                return int(parts[8]) if len(parts) > 8 else 0
    except OSError:
        return None
    return None


def _linux_proc_snapshot():
    """Return {pid: (ticks, exe_basename)} from /proc for instantaneous 1s CPU% and untruncated names."""
    snap = {}
    proc = Path('/proc')
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        try:
            stat_txt = (entry / 'stat').read_text()
            rparen = stat_txt.rfind(')')
            if rparen < 0:
                continue
            comm = stat_txt[stat_txt.find('(') + 1:rparen]
            fields = stat_txt[rparen + 2:].split()
            # fields[11]=utime, fields[12]=stime (1-based 14,15 minus 3 skipped fields)
            ticks = int(fields[11]) + int(fields[12])
            name = comm
            try:
                cmd = (entry / 'cmdline').read_bytes().split(b'\0')[0].decode('utf-8', 'replace')
                if cmd:
                    name = Path(cmd).name
            except OSError:
                pass
            snap[pid] = (ticks, name)
        except (OSError, ValueError, IndexError):
            continue
    return snap


def linux_instantaneous_table(table, prev_snap, prev_time, now_time, clk_tck=None):
    """Replace Linux lifetime-avg pcpu and 15-char comm with 1s delta CPU% and full cmdline basename."""
    cur_snap = _linux_proc_snapshot()
    dt = max(now_time - prev_time, 1e-6) if prev_time is not None else None
    tck = clk_tck or os.sysconf('SC_CLK_TCK')
    enriched = []
    for pid, ppid, rss, pcpu, comm in table:
        if pid in cur_snap:
            ticks, full_name = cur_snap[pid]
            if prev_snap is not None and pid in prev_snap and dt is not None:
                delta_ticks = max(0, ticks - prev_snap[pid][0])
                inst_cpu = (delta_ticks / tck) / dt * 100.0
            else:
                inst_cpu = 0.0
            enriched.append((pid, ppid, rss, inst_cpu, full_name))
        else:
            enriched.append((pid, ppid, rss, pcpu, comm))
    return enriched, cur_snap


def swapouts(raw):
    match = re.search(r'^Swapouts:\s+(\d+)\.?\s*$', raw, re.M)
    if match is not None:
        return int(match[1])
    match = re.search(r'^pswpout\s+(\d+)\s*$', raw, re.M)
    if match is not None:
        return int(match[1])
    raise ValueError('missing Swapouts / pswpout counter')


def observation(table, excluded, swap_raw, disk, power_state, steal_ticks=None):
    other = [(pid, cpu, name) for pid, _, _, cpu, name in table if pid not in excluded]
    native = [(pid, name) for pid, _, name in other if Path(name).name in NATIVE]
    busy = [(pid, cpu, name) for pid, cpu, name in other if cpu > 25]
    stat = os.statvfs(disk)
    obs = dict(monotonic_s=time.monotonic(), unix_s=time.time(),
               free_mib=stat.f_bavail*stat.f_frsize/1024**2,
               native=native, busy=busy, swapouts=swapouts(swap_raw), **power_state)
    if steal_ticks is not None:
        obs['steal_ticks'] = steal_ticks
    return obs


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
    prev_proc = None
    prev_proc_t = None
    prev_steal = None
    last_reason = 'no_observation'
    with Path(path).open('x') as out:
        while time.monotonic()-started < max_seconds:
            _, table, ids = harness.processes(os.getpid())
            now_t = time.monotonic()
            if sys.platform == 'linux':
                table, cur_proc = linux_instantaneous_table(table, prev_proc, prev_proc_t, now_t)
                # First Linux tick establishes baseline for 1s delta CPU; do not count toward quiet window.
                first_linux_tick = prev_proc is None
                prev_proc, prev_proc_t = cur_proc, now_t
            else:
                first_linux_tick = False
            steal = read_linux_steal()
            steal_delta = None if (steal is None or prev_steal is None) else (steal - prev_steal)
            prev_steal = steal
            sample = observation(table, ids, read_vmstat(), disk, power(), steal_ticks=steal)
            if steal_delta is not None:
                sample['steal_delta'] = steal_delta
            reason = invalid(sample, previous)
            previous = sample['swapouts']
            if sample['busy'] and reason is None:
                reason = 'competing_cpu'
            if first_linux_tick and reason is None:
                reason = 'cpu_baseline_tick'
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
        self.prev_proc = _linux_proc_snapshot() if sys.platform == 'linux' else None
        self.prev_proc_t = time.monotonic() if sys.platform == 'linux' else None
        self.prev_steal = read_linux_steal()

    def observe(self, table, ids, swap_raw):
        now_t = time.monotonic()
        if sys.platform == 'linux':
            table, self.prev_proc = linux_instantaneous_table(table, self.prev_proc, self.prev_proc_t, now_t)
            self.prev_proc_t = now_t
        steal = read_linux_steal()
        steal_delta = None if (steal is None or self.prev_steal is None) else (steal - self.prev_steal)
        self.prev_steal = steal
        sample = observation(table, ids | {os.getpid()}, swap_raw, self.disk, power(), steal_ticks=steal)
        if steal_delta is not None:
            sample['steal_delta'] = steal_delta
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
