"""The nesting an x86_64 KVM runner offers a guest (ROADMAP §11.7).

One probe, run on the host before a KVM job's first boot, so it needs no
vibeOS code: with `kvm_intel` or `kvm_amd` loaded with `nested=1`, it starts
QEMU halted (`-S`) with `-accel kvm -cpu host` and reads the `vmx` and `svm`
properties of its vCPU over QMP (`query-cpus-fast` for the vCPU's QOM path,
then `qom-get`). The answer is `vmx`, `svm` or `none`: `none` when neither
module has nesting on, or when the vCPU has neither property set.

`python3 -m tests.harness.nesting` writes `build/nesting.json` (schema 1,
`{"schema": 1, "nesting": ...}`), which `scripts/runner_info.py` copies into
the job's runner record, so the CI-history record names it (C-HISTORY).

Sources, documentation only (DESIGN §1.5): the kernel's
`Documentation/virt/kvm/x86/running-nested-guests.rst` (the `nested`
module parameter under `/sys/module/kvm_*/parameters/`), and QEMU's
`qapi/machine.json` (`query-cpus-fast`, `qom-path`) and `qapi/qom.json`
(`qom-get`).

Standard library only.
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from tests.harness.harness import HarnessError, qemu_system
from tests.harness.qmp import QmpClient, QmpError, QmpLike

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "build" / "nesting.json"
SCHEMA = 1
SYS_MODULE = Path("/sys/module")
MODULES = ("kvm_intel", "kvm_amd")
# In the order the probe reads them; the first set one is the answer.
PROPERTIES = ("vmx", "svm")
NONE = "none"
NESTINGS = (*PROPERTIES, NONE)
QUIT_WAIT_S = 10.0


def nested_modules(sys_module: Path = SYS_MODULE) -> list[str]:
    """The KVM vendor modules loaded with `nested` on (`Y` or `1`)."""
    out = []
    for m in MODULES:
        try:
            value = (sys_module / m / "parameters" / "nested").read_text(encoding="ascii")
        except (OSError, UnicodeDecodeError):
            continue
        if value.strip() in ("Y", "y", "1"):
            out.append(m)
    return out


def _property_absent(err: QmpError) -> bool:
    """QEMU's reply when the vCPU's class has no such property."""
    return "not found" in str(err)


def read_vcpu(qmp: QmpLike) -> str:
    """`vmx` or `svm`, the first of `PROPERTIES` the first vCPU has set, else
    `none`. A property the vCPU's class lacks reads as unset; any other
    error, or a reply of another shape, raises `HarnessError`."""
    cpus = qmp.execute("query-cpus-fast")
    if not isinstance(cpus, list) or not cpus or not isinstance(cpus[0], dict):
        raise HarnessError(f"nesting: query-cpus-fast returned {cpus!r}")
    path = cpus[0].get("qom-path")
    if not isinstance(path, str) or not path:
        raise HarnessError(f"nesting: query-cpus-fast has no qom-path: {cpus[0]!r}")
    for prop in PROPERTIES:
        try:
            value = qmp.execute("qom-get", {"path": path, "property": prop})
        except QmpError as e:
            if _property_absent(e):
                continue
            raise
        if not isinstance(value, bool):
            raise HarnessError(f"nesting: qom-get {prop} returned {value!r}")
        if value:
            return prop
    return NONE


def qemu_argv(binary: str, sock: str) -> list[str]:
    """A halted, deviceless `-cpu host` KVM machine with a QMP socket."""
    return [
        binary, "-S", "-accel", "kvm", "-cpu", "host", "-machine", "q35",
        "-nodefaults", "-no-user-config", "-display", "none",
        "-qmp", f"unix:{sock},server=on,wait=off",
    ]


def probe_qemu(binary: str) -> str:
    """Start `binary` halted, read its vCPU (`read_vcpu`), then quit it."""
    tmp = tempfile.mkdtemp(prefix="vibeos-nest-")
    sock = str(Path(tmp) / "qmp.sock")
    try:
        proc = subprocess.Popen(qemu_argv(binary, sock), stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    except OSError as e:
        shutil.rmtree(tmp, ignore_errors=True)
        raise HarnessError(f"nesting: cannot start {binary}: {e}") from e
    try:
        try:
            qmp = QmpClient.connect(sock)
        except QmpError as e:
            proc.kill()
            _, err = proc.communicate(timeout=QUIT_WAIT_S)
            raise HarnessError(
                f"nesting: {e}: {err.decode(errors='replace').strip()[:500]}"
            ) from e
        try:
            return read_vcpu(qmp)
        finally:
            try:
                qmp.execute("quit")
            except QmpError:
                pass
            qmp.close()
    finally:
        try:
            proc.wait(timeout=QUIT_WAIT_S)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        if proc.stderr is not None:
            proc.stderr.close()
        shutil.rmtree(tmp, ignore_errors=True)


def probe(sys_module: Path | None = None, binary: str | None = None) -> str:
    """`none` with no vendor module's nesting on, else the vCPU's answer."""
    if not nested_modules(SYS_MODULE if sys_module is None else sys_module):
        return NONE
    return probe_qemu(binary or qemu_system("x86_64"))


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    ap.add_argument("--out", type=Path, default=OUT, help="where to write the record")
    args = ap.parse_args(argv)
    try:
        nesting = probe()
    except HarnessError as e:
        print(f"nesting: {e}", file=sys.stderr)
        return 1
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps({"schema": SCHEMA, "nesting": nesting}) + "\n",
                        encoding="utf-8")
    print(f"nesting: {nesting}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
