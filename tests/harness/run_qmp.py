#!/usr/bin/env python3
"""QMP event streams and a guest core on the pinned QEMU (`make test-qmp`).

DESIGN §8.3's event rule is unit-tested by replaying QMP event streams
recorded from QEMU (`tests/harness/test_qmp.py`). This driver records them:
each scenario starts QEMU halted (`-S`) with `-device pvpanic -action
panic=pause`, and `-no-reboot` except the `reset*` ones, then drives it
through QMP and a qtest socket (`outb` to pvpanic's port 0x505 and to the
reset control register at 0xcf9). QEMU's qtest protocol runs beside TCG
here: distribution QEMU builds leave out the qtest accelerator. The
firmware is 64 KiB of `hlt`, so the CPU halts at the reset vector after
every reset and no firmware of its own resets or writes a port.

    run_qmp.py --record DIR   write the fixtures into DIR
    run_qmp.py                re-record into a temporary directory, compare
                              with tests/harness/fixtures/qmp, then boot the
                              production ISO and check one guest core

Line 1 of each `.jsonl` fixture is a header (`query-version`, argv and the
script); each further line is one event without its `timestamp`. The
compare checks event names and data, never the version. `fwcfg-q35.json`
is fw_cfg's file directory under `-machine q35`, read through qtest (select
0x19 at port 0x510, bytes from 0x511); the compare checks that it lists
`etc/pvpanic-port` with its two bytes, since the rest varies by QEMU build.

The core smoke boots `build/vibeos.iso` with QMP (`qmp.Session`), waits
for the boot contract's last marker, requires the `vibeOS: pvpanic: port`
line, takes a core (`qmp.take_core`) and checks it with `qmp.core_notes`:
ELF64 x86_64, one `CORE` `NT_PRSTATUS` note per CPU and a `VMCOREINFO` note
with `BUILD-ID=`. A failed smoke keeps its core under `build/cores/`.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from tests.harness import frame, qmp, results
from tests.harness.harness import (
    BOOT_ALLOWANCE_S,
    HarnessError,
    _start_qemu,
    boot_contract_markers,
    default_iso,
    ensure_qemu_pinned,
    env_config,
    run_failure,
    serial_tail,
)

REPO = Path(__file__).resolve().parents[2]
FIXTURES = REPO / "tests" / "harness" / "fixtures" / "qmp"
PVPANIC_PORT = 0x505
RESET_CONTROL = 0xCF9
FW_CFG_SELECTOR = 0x510
FW_CFG_DATA = 0x511
FW_CFG_FILE_DIR = 0x19
PVPANIC_FILE = "etc/pvpanic-port"
PVPANIC_LINE = "vibeOS: pvpanic: port 0x"
# How long each step waits for the events it causes.
STEP_S = 0.3

# name -> (reboot allowed, script). A step is a QMP command or a qtest line.
Script = list[tuple[str, str]]
SCENARIOS: dict[str, tuple[bool, Script]] = {
    "none": (False, [("qmp", "cont"), ("qmp", "quit")]),
    "panic": (
        False,
        [("qmp", "cont"), ("qtest", f"outb {PVPANIC_PORT:#x} 0x1"), ("qmp", "quit")],
    ),
    "reset": (
        True,
        [
            ("qmp", "cont"),
            ("qtest", f"outb {PVPANIC_PORT:#x} 0x1"),
            ("qmp", "cont"),
            ("qtest", f"outb {RESET_CONTROL:#x} 0x6"),
            ("qmp", "quit"),
        ],
    ),
    "reset-extra": (
        True,
        [
            ("qmp", "cont"),
            ("qtest", f"outb {PVPANIC_PORT:#x} 0x1"),
            ("qmp", "cont"),
            ("qtest", f"outb {RESET_CONTROL:#x} 0x6"),
            ("qtest", f"outb {RESET_CONTROL:#x} 0x6"),
            ("qmp", "quit"),
        ],
    ),
    "crashloaded": (
        False,
        [
            ("qmp", "cont"),
            ("qtest", f"outb {PVPANIC_PORT:#x} 0x2"),
            ("qtest", f"outb {RESET_CONTROL:#x} 0x6"),
        ],
    ),
}


class Qtest:
    """QEMU's qtest line protocol over a Unix socket: one command, one reply."""

    def __init__(self, path: str, timeout_s: float = 5.0) -> None:
        deadline = time.monotonic() + timeout_s
        while True:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                s.connect(path)
                break
            except OSError as e:
                s.close()
                if time.monotonic() >= deadline:
                    raise HarnessError(f"qtest: no connection to {path}: {e}") from e
                time.sleep(0.02)
        s.settimeout(10.0)
        self._sock = s
        self._buf = b""

    def cmd(self, line: str) -> str:
        self._sock.sendall(line.encode() + b"\n")
        while b"\n" not in self._buf:
            chunk = self._sock.recv(4096)
            if not chunk:
                raise HarnessError(f"qtest: closed during {line!r}")
            self._buf += chunk
        reply, _, self._buf = self._buf.partition(b"\n")
        text = reply.decode()
        if not text.startswith("OK"):
            raise HarnessError(f"qtest {line!r}: {text}")
        return text

    def inb(self, port: int) -> int:
        return int(self.cmd(f"inb {port:#x}").split()[1], 16)

    def close(self) -> None:
        self._sock.close()


# The recorder's firmware: every byte `hlt`, the reset vector included.
HLT_BIOS = b"\xf4" * 0x10000


def _argv(
    qmp_sock: str, qtest_sock: str, bios: str, *, machine: str, reboot: bool
) -> list[str]:
    argv = [
        "qemu-system-x86_64",
        "-machine", machine,
        "-accel", "tcg",
        "-bios", bios,
        "-m", "32M",
        "-display", "none",
        "-nodefaults",
        "-device", "pvpanic",
        "-action", "panic=pause",
        "-S",
        "-qmp", f"unix:{qmp_sock},server=on,wait=off",
        "-qtest", f"unix:{qtest_sock},server=on,wait=off",
    ]
    if not reboot:
        argv.append("-no-reboot")
    return argv


def _strip(ev: qmp.Event) -> qmp.Event:
    return {k: v for k, v in ev.items() if k != "timestamp"}


def _public_argv(argv: list[str]) -> list[str]:
    """`argv` with the temporary paths replaced."""
    out = []
    for prev, a in zip([""] + argv[:-1], argv, strict=True):
        if a.startswith("unix:"):
            a = "unix:<sock>" + a[a.index(",") :]
        elif prev == "-bios":
            a = "<64 KiB of hlt>"
        out.append(a)
    return out


class _Qemu:
    """A recorder's QEMU with its QMP client and qtest socket."""

    def __init__(self, *, machine: str, reboot: bool) -> None:
        self.dir = tempfile.mkdtemp(prefix="vibeos-rec-")
        qs = os.path.join(self.dir, "qmp.sock")
        ts = os.path.join(self.dir, "qtest.sock")
        bios = os.path.join(self.dir, "hlt.bin")
        Path(bios).write_bytes(HLT_BIOS)
        self.argv = _argv(qs, ts, bios, machine=machine, reboot=reboot)
        ensure_qemu_pinned(self.argv[0], env_config(
            default_iso=default_iso(), default_timeout=BOOT_ALLOWANCE_S
        ).qemu_version or None)
        self.proc = subprocess.Popen(
            self.argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        try:
            self.qmp = qmp.QmpClient.connect(qs)
            self.qtest = Qtest(ts)
        except BaseException:
            self.proc.kill()
            self.proc.wait()
            raise

    def version(self) -> object:
        return self.qmp.execute("query-version")

    def close(self) -> None:
        self.qtest.close()
        self.qmp.close()
        if self.proc.poll() is None:
            self.proc.kill()
        self.proc.wait()
        if self.proc.stderr is not None:
            self.proc.stderr.close()
        shutil.rmtree(self.dir, ignore_errors=True)


def record_stream(name: str) -> tuple[dict[str, object], list[qmp.Event]]:
    reboot, script = SCENARIOS[name]
    q = _Qemu(machine="pc", reboot=reboot)
    events: list[qmp.Event] = []
    try:
        header: dict[str, object] = {
            "query-version": q.version(),
            "argv": _public_argv(q.argv),
            "script": [f"{kind} {step}" for kind, step in script],
        }
        for kind, step in script:
            if kind == "qmp":
                try:
                    q.qmp.execute(step)
                except qmp.QmpError:
                    if step != "quit":
                        raise
            else:
                q.qtest.cmd(step)
            time.sleep(STEP_S)
            events += q.qmp.poll()
        try:
            q.proc.wait(timeout=5.0)
        except subprocess.TimeoutExpired:
            pass
        events += q.qmp.poll()
    finally:
        q.close()
    return header, [_strip(e) for e in events]


def record_fwcfg_q35() -> dict[str, object]:
    q = _Qemu(machine="q35", reboot=False)
    try:
        header: dict[str, object] = {
            "query-version": q.version(),
            "argv": _public_argv(q.argv),
            "script": [f"qtest outw {FW_CFG_SELECTOR:#x} {FW_CFG_FILE_DIR:#x}",
                       f"qtest inb {FW_CFG_DATA:#x} (repeated)"],
        }
        q.qtest.cmd(f"outw {FW_CFG_SELECTOR:#x} {FW_CFG_FILE_DIR:#x}")

        def read(n: int) -> bytes:
            return bytes(q.qtest.inb(FW_CFG_DATA) for _ in range(n))

        count = int.from_bytes(read(4), "big")
        files = []
        for _ in range(min(count, 1024)):
            e = read(64)
            files.append(
                {
                    "name": e[8:].split(b"\0", 1)[0].decode("ascii", errors="replace"),
                    "select": int.from_bytes(e[4:6], "big"),
                    "size": int.from_bytes(e[0:4], "big"),
                }
            )
    finally:
        q.close()
    return {"header": header, "files": files}


def pvpanic_entry(fwcfg: dict[str, object]) -> dict[str, object] | None:
    files = fwcfg.get("files")
    for f in files if isinstance(files, list) else []:
        if isinstance(f, dict) and f.get("name") == PVPANIC_FILE:
            return f
    return None


def record(out: Path) -> dict[str, object]:
    """Record every fixture into `out`; the header's version."""
    out.mkdir(parents=True, exist_ok=True)
    version: object = None
    for name in SCENARIOS:
        header, events = record_stream(name)
        version = header["query-version"]
        lines = [json.dumps(header, sort_keys=True)] + [
            json.dumps(e, sort_keys=True) for e in events
        ]
        (out / f"{name}.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")
    fw = record_fwcfg_q35()
    (out / "fwcfg-q35.json").write_text(
        json.dumps(fw, indent=1, sort_keys=True) + "\n", encoding="utf-8"
    )
    return {"version": version}


def compare(fresh: Path, committed: Path) -> list[str]:
    """One message per fixture whose events differ from the committed one."""
    errors: list[str] = []
    for name in SCENARIOS:
        path = committed / f"{name}.jsonl"
        if not path.is_file():
            errors.append(f"{path}: missing (record with run_qmp.py --record)")
            continue
        _, want = qmp.load_stream(path)
        _, got = qmp.load_stream(fresh / f"{name}.jsonl")
        if got != want:
            errors.append(
                f"{name}: events differ from {path.relative_to(REPO)}:\n"
                f"  recorded now: {json.dumps(got)}\n  committed:    {json.dumps(want)}"
            )
    for label, p in (("recorded now", fresh), ("committed", committed)):
        fw = json.loads((p / "fwcfg-q35.json").read_text(encoding="utf-8"))
        e = pvpanic_entry(fw)
        if e is None or e.get("size") != 2:
            errors.append(f"fwcfg-q35 ({label}): no {PVPANIC_FILE} of 2 bytes: {e!r}")
    return errors


def core_smoke(res: results.Results) -> None:
    """Boot the production ISO, take a core at the contract's end, check it."""
    env = env_config(default_iso=default_iso(), default_timeout=BOOT_ALLOWANCE_S)
    cfg = env.qemu()
    markers = boot_contract_markers(cpu=env.cpu, smp=env.smp)
    session = qmp.Session(cfg, "smoke")
    src = _start_qemu(cfg, time.monotonic() + env.timeout, qmp_sock=session.sock)
    stream = frame.Stream()
    lines: list[str] = []
    idx = 0
    pvpanic = ""
    keep = Path(tempfile.mkdtemp(prefix="vibeos-core-"))
    try:
        session.start()
        while idx < len(markers):
            kind, line = src.next_event()
            if kind == "idle":
                continue
            if kind in ("timeout", "eof"):
                raise HarnessError(
                    f"core smoke: {kind} before marker {markers[idx].name!r}{serial_tail(lines)}"
                )
            lines.append(line)
            why = run_failure(line, stream)
            if why is not None:
                raise HarnessError(f"core smoke: {why[0]} in: {why[1]!r}")
            text = frame.kernel_text(line) or ""
            if text.startswith(PVPANIC_LINE):
                pvpanic = text
            if markers[idx].matches(line):
                idx += 1
        if not pvpanic:
            raise HarnessError(f"core smoke: no {PVPANIC_LINE!r} line{serial_tail(lines)}")
        print(f"[qmp] {pvpanic}", file=sys.stderr)
        assert session.qmp is not None
        session.qmp.execute("stop")
        t0 = time.monotonic()
        core = qmp.take_core(session.qmp, keep)
        took = time.monotonic() - t0
        notes = qmp.core_notes(core)
        prstatus = [n for n in notes if n[0] == "CORE" and n[1] == qmp.NT_PRSTATUS]
        if len(prstatus) != cfg.smp:
            raise HarnessError(
                f"core smoke: {len(prstatus)} CORE NT_PRSTATUS notes, want {cfg.smp} (one per CPU)"
            )
        info = [n for n in notes if n[0] == "VMCOREINFO"]
        if len(info) != 1 or b"BUILD-ID=" not in info[0][2]:
            raise HarnessError(f"core smoke: VMCOREINFO notes {info!r}, want one with BUILD-ID=")
        print(
            f"[qmp] core: {core.stat().st_size} bytes zstd in {took:.1f} s, "
            f"{len(prstatus)} NT_PRSTATUS, VMCOREINFO with BUILD-ID=",
            file=sys.stderr,
        )
    except HarnessError:
        if (keep / "core.zst").is_file():
            d = qmp.run_dir("smoke")
            shutil.copyfile(keep / "core.zst", d / "core.zst")
            qmp.save_run_files(d, src.argv, cfg.iso)
        raise
    finally:
        session.ended = "pass"
        session.close()
        if src.wait(5.0) is None:
            src.kill()
            src.wait(5.0)
        res.add_boot(src.argv, cfg, src.wait(0.0))
        shutil.rmtree(keep, ignore_errors=True)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    ap.add_argument("--record", type=Path, metavar="DIR", help="write the fixtures into DIR")
    args = ap.parse_args()
    if args.record is not None:
        info = record(args.record)
        version = json.dumps(info["version"])
        print(f"[qmp] recorded into {args.record} on {version}", file=sys.stderr)
        return 0
    env = env_config(default_iso=default_iso(), default_timeout=BOOT_ALLOWANCE_S)
    res = results.Results(env.tier)
    with tempfile.TemporaryDirectory(prefix="vibeos-qmp-rec-") as d:
        try:
            info = record(Path(d))
        except (HarnessError, OSError) as e:
            res.record("marker", "qmp_streams", "failed")
            print(f"[qmp] FAIL: recording: {e}", file=sys.stderr)
            return 1
        print(f"[qmp] recorded on {json.dumps(info['version'])}", file=sys.stderr)
        errors = compare(Path(d), FIXTURES)
    if errors:
        res.record("marker", "qmp_streams", "failed")
        for msg in errors:
            print(f"[qmp] FAIL: {msg}", file=sys.stderr)
        return 1
    res.record("marker", "qmp_streams", "passed")
    print(
        f"[qmp] ok: {len(SCENARIOS)} streams match, q35 lists {PVPANIC_FILE}", file=sys.stderr
    )
    try:
        core_smoke(res)
    except HarnessError as e:
        res.record("marker", "qmp_core_smoke", "failed")
        print(f"[qmp] FAIL: {e}", file=sys.stderr)
        return 1
    res.record("marker", "qmp_core_smoke", "passed")
    print("[qmp] ok: core smoke", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(results.run_main(main))
