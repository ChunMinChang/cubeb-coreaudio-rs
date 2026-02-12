"""LLDB Python module and standalone analyzer for HALB_Mutex tracing.

LLDB mode (live tracing):
  lldb -o "command script import trace_mutex.py" ...

Standalone mode (analyze saved log):
  python3 trace_mutex.py <logfile>

Sets breakpoints on:
  - HALB_Mutex::Lock/Unlock (primary) or pthread_mutex_lock/unlock (fallback)
  - CoreAudio sync operations: AudioOutputUnitStop, AudioObjectRemovePropertyListener,
    AudioComponentInstanceDispose, AudioUnitUninitialize
  - All callback functions in the test binary (regex: "callback")
  - AudioOutputUnitStart, AudioObjectAddPropertyListener, AudioUnitInitialize
  - User-specified extra patterns via TRACE_EXTRA_PATTERNS env var
"""

from collections import defaultdict
import os
import re
import sys
import time

_start_time = None
_events = []
_halb_available = False
_inside_sync = {}
_bp_pthread_lock = None
_bp_pthread_unlock = None

SYNC_OPS = [
    "AudioOutputUnitStop",
    "AudioObjectRemovePropertyListener",
    "AudioComponentInstanceDispose",
    "AudioUnitUninitialize",
]

LIFECYCLE_OPS = [
    "AudioOutputUnitStart",
    "AudioObjectAddPropertyListener",
    "AudioUnitInitialize",
]

TRACE_RE = re.compile(
    r"TRACE:\s+(\d+\.\d+)s\s+T(\d+)\s+(\S+(?:\s+\S+)*?)"
    r"(?:\s+(0x[0-9a-f]+|\?))?$"
)


def _ts():
    return time.time() - _start_time


def _record(frame, event_name, extra=""):
    tid = frame.GetThread().GetIndexID()
    ts = _ts()
    _events.append((ts, tid, event_name, extra))
    print(f"TRACE: {ts:10.6f}s  T{tid:<4d}  {event_name}  {extra}", flush=True)


def _get_mutex_addr(frame):
    reg = frame.FindRegister("x0")
    if reg.IsValid():
        return f"0x{reg.GetValueAsUnsigned():x}"
    return "?"


def _short_fn(frame):
    fn = frame.GetFunctionName() or "unknown"
    if "::" in fn:
        parts = fn.split("::")
        for p in reversed(parts):
            if "callback" in p.lower() or "listener" in p.lower():
                return p
        return parts[-1]
    return fn


# -- HALB_Mutex callbacks --

def on_halb_lock(frame, bp_loc, dict):
    _record(frame, "HALB_Mutex::Lock", _get_mutex_addr(frame))
    return False


def on_halb_unlock(frame, bp_loc, dict):
    _record(frame, "HALB_Mutex::Unlock", _get_mutex_addr(frame))
    return False


# -- Fallback: pthread callbacks --

def on_pthread_lock(frame, bp_loc, dict):
    _record(frame, "pthread_mutex_lock", _get_mutex_addr(frame))
    return False


def on_pthread_unlock(frame, bp_loc, dict):
    _record(frame, "pthread_mutex_unlock", _get_mutex_addr(frame))
    return False


# -- Sync operations --

def on_sync_enter(frame, bp_loc, dict):
    global _bp_pthread_lock, _bp_pthread_unlock
    fn = frame.GetFunctionName() or "unknown"
    tid = frame.GetThread().GetIndexID()
    _record(frame, f"{fn} ENTER")
    _inside_sync[tid] = fn

    # Set return breakpoint to capture sync op duration
    caller = frame.GetThread().GetFrameAtIndex(1)
    if caller and caller.GetPC() != 0:
        target = frame.GetThread().GetProcess().GetTarget()
        bp_ret = target.BreakpointCreateByAddress(caller.GetPC())
        bp_ret.SetOneShot(True)
        bp_ret.SetScriptCallbackFunction("trace_mutex.on_sync_return")

    # Enable pthread fallback tracing during sync ops
    if not _halb_available and _bp_pthread_lock is not None:
        _bp_pthread_lock.SetEnabled(True)
        _bp_pthread_unlock.SetEnabled(True)

    return False


def on_sync_return(frame, bp_loc, dict):
    global _bp_pthread_lock, _bp_pthread_unlock
    tid = frame.GetThread().GetIndexID()
    fn = _inside_sync.pop(tid, "unknown")
    _record(frame, f"{fn} RETURN")

    if not _halb_available and not _inside_sync:
        if _bp_pthread_lock is not None:
            _bp_pthread_lock.SetEnabled(False)
            _bp_pthread_unlock.SetEnabled(False)

    return False


# -- Lifecycle operations --

def on_lifecycle(frame, bp_loc, dict):
    fn = frame.GetFunctionName() or "unknown"
    _record(frame, f"{fn} ENTER")
    return False


# -- Callbacks --

def on_callback(frame, bp_loc, dict):
    _record(frame, f"callback({_short_fn(frame)})")
    return False


# -- Extra user-specified breakpoints --

def on_extra(frame, bp_loc, dict):
    _record(frame, f"extra({_short_fn(frame)})")
    return False


# ============================================================
# Analysis (shared between LLDB summary and standalone mode)
# ============================================================

def analyze(events, halb_available=True):
    """Analyze trace events and return report lines.

    Args:
        events: list of (ts, tid, event_name, extra) tuples
        halb_available: whether HALB_Mutex symbols were resolved directly
    """
    lines = []
    lines.append("")
    lines.append("=" * 70)
    lines.append("HALB_Mutex TRACE SUMMARY")
    lines.append("=" * 70)
    lines.append("")
    lines.append(f"Symbol resolution: {'HALB_Mutex (direct)' if halb_available else 'pthread fallback'}")
    lines.append(f"Total events: {len(events)}")

    # Timeline
    lines.append("")
    lines.append("--- Event Timeline ---")
    lines.append(f"{'Time':>12s}  {'Thread':>6s}  Event")
    lines.append("-" * 70)
    for ts, tid, event, extra in events:
        extra_str = f"  {extra}" if extra else ""
        lines.append(f"{ts:12.6f}s  T{tid:<4d}  {event}{extra_str}")

    # Basic stats
    lines.append("")
    lines.append("--- Analysis ---")

    mutex_events = [(ts, tid, ev, ex) for ts, tid, ev, ex in events
                    if "Lock" in ev or "Unlock" in ev]
    lock_count = sum(1 for _, _, ev, _ in mutex_events if "Unlock" not in ev and "Lock" in ev)
    unlock_count = sum(1 for _, _, ev, _ in mutex_events if "Unlock" in ev)

    # Build paired sync operations (enter + return)
    sync_enters = [(ts, tid, ev) for ts, tid, ev, _ in events
                   if "ENTER" in ev and any(op in ev for op in SYNC_OPS)]
    sync_returns = [(ts, tid, ev) for ts, tid, ev, _ in events if "RETURN" in ev]
    sync_pairs = []
    for enter_ts, enter_tid, enter_ev in sync_enters:
        op_name = enter_ev.replace(" ENTER", "")
        ret_ts = None
        for rts, rtid, rev in sync_returns:
            if rtid == enter_tid and rts > enter_ts and op_name in rev:
                ret_ts = rts
                break
        has_return = ret_ts is not None
        if ret_ts is None:
            ret_ts = events[-1][0]
        sync_pairs.append((op_name, enter_ts, ret_ts, enter_tid, has_return))

    # Group sync ops by type
    sync_by_op = defaultdict(list)
    for op_name, enter_ts, ret_ts, enter_tid, has_return in sync_pairs:
        dur_ms = (ret_ts - enter_ts) * 1000
        mutex_during = sum(1 for ts, _, ev, _ in mutex_events if enter_ts <= ts <= ret_ts)
        sync_by_op[op_name].append({
            "enter_ts": enter_ts, "ret_ts": ret_ts, "tid": enter_tid,
            "dur_ms": dur_ms, "mutex_ops": mutex_during, "has_return": has_return,
        })

    for op_name in SYNC_OPS:
        if op_name not in sync_by_op:
            continue
        calls = sync_by_op[op_name]
        timed = [c for c in calls if c["has_return"]]
        if timed:
            avg_ms = sum(c["dur_ms"] for c in timed) / len(timed)
            total_mutex = sum(c["mutex_ops"] for c in timed)
            lines.append(f"{op_name}: {len(calls)} call(s), "
                         f"{len(timed)} timed, avg={avg_ms:.1f}ms, mutex_ops={total_mutex}")
        else:
            lines.append(f"{op_name}: {len(calls)} call(s), 0 timed (no return detected)")

    lines.append(f"Total Lock events: {lock_count}")
    lines.append(f"Total Unlock events: {unlock_count}")

    # Thread role detection
    callback_threads = set(tid for _, tid, ev, _ in events if ev.startswith("callback("))
    sync_threads = set()
    for _, tid, ev, _ in events:
        for op in SYNC_OPS:
            if f"{op} ENTER" in ev:
                sync_threads.add(tid)

    # Per-mutex instance analysis
    mutex_by_addr = {}
    for ts, tid, ev, extra in events:
        if ("Lock" in ev or "Unlock" in ev) and extra and extra.startswith("0x"):
            addr = extra
            if addr not in mutex_by_addr:
                mutex_by_addr[addr] = {"threads": set(), "count": 0, "events": []}
            mutex_by_addr[addr]["threads"].add(tid)
            mutex_by_addr[addr]["count"] += 1
            mutex_by_addr[addr]["events"].append((ts, tid, ev))

    shared_addrs = []
    if mutex_by_addr:
        lines.append("")
        lines.append("--- Mutex Instances ---")
        lines.append(f"Distinct mutex addresses: {len(mutex_by_addr)}")
        if callback_threads:
            lines.append(f"Callback thread(s): {', '.join(f'T{t}' for t in sorted(callback_threads))}")
        if sync_threads:
            lines.append(f"Sync thread(s): {', '.join(f'T{t}' for t in sorted(sync_threads))}")
        lines.append("")

        for addr in sorted(mutex_by_addr, key=lambda a: mutex_by_addr[a]["count"], reverse=True):
            info = mutex_by_addr[addr]
            tset = info["threads"]
            is_cb = bool(tset & callback_threads)
            is_sync = bool(tset & sync_threads)
            shared = is_cb and is_sync
            tag = " [SHARED: callback + sync]" if shared else ""
            threads_str = ", ".join(f"T{t}" for t in sorted(tset))
            lines.append(f"  {addr}: {info['count']:>6d} ops, threads=[{threads_str}]{tag}")
            if shared:
                shared_addrs.append(addr)

        # Contention timeline — one representative window per sync op type
        if shared_addrs and sync_pairs:
            lines.append("")
            lines.append("--- Contention Timeline (shared mutex during sync ops) ---")
            for addr in shared_addrs:
                lines.append(f"")
                lines.append(f"Mutex {addr}:")
                shown_ops = set()
                for op_name, enter_ts, ret_ts, enter_tid, has_return in sync_pairs:
                    if op_name in shown_ops:
                        continue
                    window_start = enter_ts - 0.5
                    relevant = [(ts, tid, ev) for ts, tid, ev in mutex_by_addr[addr]["events"]
                                if window_start <= ts <= ret_ts]
                    if not relevant:
                        continue
                    window_tids = set(tid for _, tid, _ in relevant)
                    if not (window_tids & callback_threads) or not (window_tids & sync_threads):
                        continue
                    shown_ops.add(op_name)
                    ret_tag = "" if has_return else " (estimated)"
                    lines.append(f"  {op_name} (T{enter_tid}, {enter_ts:.3f}s - {ret_ts:.3f}s{ret_tag}):")
                    if len(relevant) > 40:
                        for ts, tid, ev in relevant[:20]:
                            marker = ""
                            if tid in callback_threads:
                                marker = " <-- callback"
                            elif tid in sync_threads:
                                marker = " <-- sync"
                            lines.append(f"    {ts:12.6f}s  T{tid:<4d}  {ev}{marker}")
                        lines.append(f"    ... ({len(relevant) - 30} more events) ...")
                        for ts, tid, ev in relevant[-10:]:
                            marker = ""
                            if tid in callback_threads:
                                marker = " <-- callback"
                            elif tid in sync_threads:
                                marker = " <-- sync"
                            lines.append(f"    {ts:12.6f}s  T{tid:<4d}  {ev}{marker}")
                    else:
                        for ts, tid, ev in relevant:
                            marker = ""
                            if tid in callback_threads:
                                marker = " <-- callback"
                            elif tid in sync_threads:
                                marker = " <-- sync"
                            lines.append(f"    {ts:12.6f}s  T{tid:<4d}  {ev}{marker}")

    # Race window analysis — grouped by sync op type
    if sync_pairs:
        race_by_op = defaultdict(list)
        for op_name, enter_ts, ret_ts, enter_tid, has_return in sync_pairs:
            window_start = enter_ts - 1.0
            cbs_in_window = [(ts, tid, ev) for ts, tid, ev, _ in events
                             if ev.startswith("callback(")
                             and tid != enter_tid
                             and window_start <= ts <= ret_ts]
            if cbs_in_window:
                race_by_op[op_name].append({
                    "enter_ts": enter_ts, "ret_ts": ret_ts, "tid": enter_tid,
                    "has_return": has_return, "callbacks": cbs_in_window,
                })

        if race_by_op:
            lines.append("")
            lines.append("--- Race Windows ---")
            for op_name in SYNC_OPS:
                if op_name not in race_by_op:
                    continue
                windows = race_by_op[op_name]
                total_calls = len(sync_by_op.get(op_name, []))
                lines.append(f"{op_name}: {len(windows)} of {total_calls} call(s) had callbacks in window")

                w = windows[0]
                ret_tag = "" if w["has_return"] else " (estimated)"
                lines.append(f"  Representative (T{w['tid']}, {w['enter_ts']:.3f}s - {w['ret_ts']:.3f}s{ret_tag}):")
                for ts, tid, ev in w["callbacks"][:5]:
                    if ts < w["enter_ts"]:
                        lines.append(f"    T{tid} {ev} at {ts:.3f}s  <-- in-flight")
                    else:
                        lines.append(f"    T{tid} {ev} at {ts:.3f}s  <-- during {op_name}")
                if len(w["callbacks"]) > 5:
                    lines.append(f"    ... ({len(w['callbacks']) - 5} more callbacks)")

                if shared_addrs:
                    for addr in shared_addrs:
                        events_during = [(ts, tid, ev) for ts, tid, ev
                                         in mutex_by_addr[addr]["events"]
                                         if w["enter_ts"] <= ts <= w["ret_ts"]]
                        for j in range(len(events_during) - 1):
                            ts1, tid1, ev1 = events_during[j]
                            ts2, tid2, ev2 = events_during[j + 1]
                            if (tid1 in callback_threads and "Unlock" in ev1 and
                                    tid2 in sync_threads and "Lock" in ev2):
                                lines.append(f"    Mutex {addr}: T{tid1} Unlock -> T{tid2} Lock at {ts2:.3f}s (handoff)")
                                break

    # Synchronization evidence — detailed handoff timing on shared mutexes
    if shared_addrs and sync_pairs:
        lines.append("")
        lines.append("--- Synchronization Evidence ---")
        lines.append("Handoff = Thread A Unlock(M) followed by Thread B Lock(M)")
        lines.append("Small gap suggests Thread B was blocked waiting for the mutex.")
        lines.append("")

        for op_name in SYNC_OPS:
            if op_name not in sync_by_op:
                continue

            # Find a representative call that has shared mutex activity
            best = None
            best_handoffs = []
            for call in sync_by_op[op_name]:
                enter_ts, ret_ts = call["enter_ts"], call["ret_ts"]
                handoffs = []
                for addr in shared_addrs:
                    evts = [(ts, tid, ev) for ts, tid, ev in mutex_by_addr[addr]["events"]
                            if enter_ts <= ts <= ret_ts]
                    for j in range(len(evts) - 1):
                        ts1, tid1, ev1 = evts[j]
                        ts2, tid2, ev2 = evts[j + 1]
                        if "Unlock" not in ev1 or "Lock" not in ev2 or "Unlock" in ev2:
                            continue
                        if tid1 == tid2:
                            continue
                        if tid1 in callback_threads and tid2 in sync_threads:
                            gap_us = (ts2 - ts1) * 1e6
                            handoffs.append((addr, ts1, tid1, ts2, tid2, gap_us, "callback->sync"))
                        elif tid1 in sync_threads and tid2 in callback_threads:
                            gap_us = (ts2 - ts1) * 1e6
                            handoffs.append((addr, ts1, tid1, ts2, tid2, gap_us, "sync->callback"))
                if len(handoffs) > len(best_handoffs):
                    best = call
                    best_handoffs = handoffs

            if not best_handoffs:
                continue

            ret_tag = "" if best["has_return"] else " (estimated)"
            lines.append(f"{op_name} (T{best['tid']}, {best['enter_ts']:.3f}s - {best['ret_ts']:.3f}s{ret_tag}):")

            cb_to_sync = [h for h in best_handoffs if h[6] == "callback->sync"]
            sync_to_cb = [h for h in best_handoffs if h[6] == "sync->callback"]

            if cb_to_sync:
                gaps = [h[5] for h in cb_to_sync]
                lines.append(f"  callback->sync handoffs: {len(cb_to_sync)}, "
                             f"gap min={min(gaps):.0f}us, max={max(gaps):.0f}us, avg={sum(gaps)/len(gaps):.0f}us")
                for addr, ts1, tid1, ts2, tid2, gap_us, _ in cb_to_sync[:5]:
                    lines.append(f"    {addr}: T{tid1} Unlock {ts1:.6f}s -> T{tid2} Lock {ts2:.6f}s  (gap {gap_us:.0f}us)")
                if len(cb_to_sync) > 5:
                    lines.append(f"    ... ({len(cb_to_sync) - 5} more)")

            if sync_to_cb:
                gaps = [h[5] for h in sync_to_cb]
                lines.append(f"  sync->callback handoffs: {len(sync_to_cb)}, "
                             f"gap min={min(gaps):.0f}us, max={max(gaps):.0f}us, avg={sum(gaps)/len(gaps):.0f}us")
                for addr, ts1, tid1, ts2, tid2, gap_us, _ in sync_to_cb[:5]:
                    lines.append(f"    {addr}: T{tid1} Unlock {ts1:.6f}s -> T{tid2} Lock {ts2:.6f}s  (gap {gap_us:.0f}us)")
                if len(sync_to_cb) > 5:
                    lines.append(f"    ... ({len(sync_to_cb) - 5} more)")

            lines.append("")

    # Conclusion
    if lock_count > 0:
        lines.append("")
        if shared_addrs:
            lines.append(f"CONCLUSION: {len(shared_addrs)} mutex instance(s) observed on both")
            lines.append("callback thread(s) and sync thread(s), consistent with HALB_Mutex-based")
            lines.append("internal synchronization.")
        else:
            lines.append("CONCLUSION: Mutex activity observed but no single mutex instance was")
            lines.append("found on both callback and sync threads. Synchronization may use a")
            lines.append("different mechanism or the callback was not detected.")
    else:
        lines.append("")
        lines.append("WARNING: No mutex lock/unlock events captured.")
        lines.append("Try running with a different test or check LLDB symbol resolution.")

    return lines


# ============================================================
# Log file parsing (for standalone analysis)
# ============================================================

def parse_log(path):
    """Parse TRACE lines from a log file into event tuples."""
    events = []
    halb_available = True
    with open(path) as f:
        for line in f:
            line = line.rstrip()
            if "Symbol resolution: pthread fallback" in line:
                halb_available = False
            m = TRACE_RE.match(line)
            if not m:
                continue
            ts = float(m.group(1))
            tid = int(m.group(2))
            rest = m.group(3)
            extra = m.group(4) or ""
            # Detect halb from the event names
            if "pthread_mutex" in rest:
                halb_available = False
            events.append((ts, tid, rest, extra))
    return events, halb_available


# ============================================================
# LLDB summary command
# ============================================================

def cmd_summary(debugger, command, result, internal_dict):
    if not _events:
        result.AppendMessage("\n[trace] No events captured.\n")
        return

    report = analyze(_events, _halb_available)
    output = "\n".join(report) + "\n"
    print(output)
    result.AppendMessage(output)


# ============================================================
# LLDB module init
# ============================================================

def __lldb_init_module(debugger, internal_dict):
    import lldb as _lldb

    global _start_time, _halb_available, _bp_pthread_lock, _bp_pthread_unlock
    _start_time = time.time()

    target = debugger.GetSelectedTarget()
    if not target:
        print("[trace] ERROR: No target selected.")
        return

    print("")
    print("=" * 60)
    print("  HALB_Mutex Tracer (general-purpose)")
    print("=" * 60)
    print("")

    # 1. HALB_Mutex breakpoints (primary) or pthread fallback
    bp_lock = target.BreakpointCreateByRegex("HALB_Mutex::Lock")
    bp_unlock = target.BreakpointCreateByRegex("HALB_Mutex::Unlock")
    halb_lock_locs = bp_lock.GetNumLocations()
    halb_unlock_locs = bp_unlock.GetNumLocations()

    if halb_lock_locs > 0 or halb_unlock_locs > 0:
        _halb_available = True
        print(f"  HALB_Mutex::Lock   : {halb_lock_locs} location(s) [RESOLVED]")
        print(f"  HALB_Mutex::Unlock : {halb_unlock_locs} location(s) [RESOLVED]")
        bp_lock.SetScriptCallbackFunction("trace_mutex.on_halb_lock")
        bp_unlock.SetScriptCallbackFunction("trace_mutex.on_halb_unlock")
    else:
        _halb_available = False
        print("  HALB_Mutex::Lock   : 0 locations [NOT RESOLVED]")
        print("  HALB_Mutex::Unlock : 0 locations [NOT RESOLVED]")
        print("  -> Using pthread_mutex_lock/unlock fallback (gated by sync ops)")
        bp_lock.SetEnabled(False)
        bp_unlock.SetEnabled(False)

        _bp_pthread_lock = target.BreakpointCreateByName("pthread_mutex_lock")
        _bp_pthread_unlock = target.BreakpointCreateByName("pthread_mutex_unlock")
        _bp_pthread_lock.SetScriptCallbackFunction("trace_mutex.on_pthread_lock")
        _bp_pthread_unlock.SetScriptCallbackFunction("trace_mutex.on_pthread_unlock")
        _bp_pthread_lock.SetEnabled(False)
        _bp_pthread_unlock.SetEnabled(False)
        print(f"  pthread_mutex_lock : {_bp_pthread_lock.GetNumLocations()} location(s) [DISABLED]")
        print(f"  pthread_mutex_unlock: {_bp_pthread_unlock.GetNumLocations()} location(s) [DISABLED]")

    # 2. Sync operation breakpoints
    for op in SYNC_OPS:
        bp = target.BreakpointCreateByName(op)
        bp.SetScriptCallbackFunction("trace_mutex.on_sync_enter")
        print(f"  {op}: {bp.GetNumLocations()} location(s) [sync]")

    # 3. Lifecycle operation breakpoints
    for op in LIFECYCLE_OPS:
        bp = target.BreakpointCreateByName(op)
        bp.SetScriptCallbackFunction("trace_mutex.on_lifecycle")
        print(f"  {op}: {bp.GetNumLocations()} location(s) [lifecycle]")

    # 4. Callback breakpoints — regex "callback" restricted to test binary
    exe_module = target.GetExecutable()
    module_list = _lldb.SBFileSpecList()
    if exe_module.IsValid():
        module_list.Append(exe_module)
    comp_unit_list = _lldb.SBFileSpecList()
    bp_cb = target.BreakpointCreateByRegex("callback", module_list, comp_unit_list)
    bp_cb.SetScriptCallbackFunction("trace_mutex.on_callback")
    print(f"  *callback* (binary): {bp_cb.GetNumLocations()} location(s) [callback]")

    # 5. Extra breakpoint patterns from env var
    extra = os.environ.get("TRACE_EXTRA_PATTERNS", "")
    if extra:
        for pattern in extra.split(","):
            pattern = pattern.strip()
            if not pattern:
                continue
            bp_extra = target.BreakpointCreateByRegex(pattern)
            bp_extra.SetScriptCallbackFunction("trace_mutex.on_extra")
            print(f"  {pattern}: {bp_extra.GetNumLocations()} location(s) [extra]")

    # Register summary command
    debugger.HandleCommand(
        "command script add -f trace_mutex.cmd_summary trace_summary"
    )

    print("")
    print("Tracing active. Starting test...")
    print("")


# ============================================================
# Standalone entry point
# ============================================================

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(f"Usage: python3 {sys.argv[0]} <logfile>")
        print("")
        print("Analyze a saved trace log from trace_mutex.sh.")
        sys.exit(1)

    logfile = sys.argv[1]
    if not os.path.isfile(logfile):
        print(f"ERROR: File not found: {logfile}")
        sys.exit(1)

    events, halb = parse_log(logfile)
    if not events:
        print(f"No TRACE events found in {logfile}")
        sys.exit(1)

    print(f"Parsed {len(events)} events from {logfile}")
    report = analyze(events, halb)
    print("\n".join(report))
