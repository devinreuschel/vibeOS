# T2 · One QEMU launcher for all drivers

| | |
|---|---|
| **Area** | 4.6 Testing |
| **Impact / Effort / Phase** | Medium / S / I |
| **Depends on** | — |
| **Blocks** | C2, T3, D2 (device presets) |
| **Review** | [ARCHITECTURE_REVIEW.md §4.6](../ARCHITECTURE_REVIEW.md#46-testing-strategy--coverage) |
| **Status** | In the [index](README.md). #79. |

## Problem

- QEMU argument construction exists three times: `tests/harness/harness.py::_qemu_argv` (monitor socket, accel, HPET), `tests/kernel_boot.py::_blk_extra` (virtio-blk drive, `-boot order=d`), `tests/vibefs_crash.py::_qemu_argv` (its own accel/bios handling, no monitor).
- `vibefs_crash.py` redefines the panic-signature list (`PANIC`, adding `"vibeOS: vibefs: mount fail"`) and its own `select`-based line loop instead of `DeadlineReader`.
- `tests/e2e/` is an empty directory holding only an untracked `__pycache__` (a moved file); `tests/harness/run_ps2.py` was added by #67 alongside `run_e2e.py`, while `kernel_boot.py` and `vibefs_crash.py` sit one level up.

## Recommended fix

Everything that launches QEMU lives in `tests/harness/harness.py`; every driver lives in `tests/harness/run_*.py`.

## Implementation plan

1. **Presets in `harness.py`:**
   ```python
   def make_disk(nbytes: int, prefix: str) -> str          # tempfile + ftruncate, caller unlinks
   def virtio_blk_args(disk: str, smp: int, *, discard: bool = True) -> tuple[str, ...]
   def ktest_devices(disk: str, smp: int) -> tuple[str, ...]   # isa-debug-exit + e1000e + edu + virtio-rng + virtio_blk_args
   def qemu_argv(cfg: QemuConfig, monitor_sock: str | None) -> list[str]   # public
   ```
   `QemuConfig` gains `boot_order: str | None` and `extra_panic: tuple[str, ...]`.
2. **`run_qemu_until_exit` / `run_qemu_and_check`** accept `extra_panic` so the crash driver adds `"vibeOS: vibefs: mount fail"` without a second list.
3. **Move and rename:** `tests/kernel_boot.py` → `tests/harness/run_ktest.py`; `tests/vibefs_crash.py` → `tests/harness/run_vibefs_crash.py` rewritten on `DeadlineReader` + `run_qemu_until_exit`-style loop with a `kill_after` callback (the randomized SIGKILL is the only special behaviour). Delete `tests/e2e/`.
4. **Makefile:** update the four `python3 tests/…` paths.
5. **Harness tests:** `ktest_devices` contains `disable-legacy=on` and `num-queues=<smp>`; `virtio_blk_args` without discard omits `discard=unmap`; the crash driver's kill window stays within `[0, 0.18]` s (pure function `kill_delay(rng)`).

## Acceptance criteria

- `grep -rn 'qemu-system-x86_64' tests` matches only `harness.py`.
- `grep -rn 'PANIC' tests | grep -v harness.py` is empty.
- `ls tests` shows `harness/`, `hostlib/` only.

## Tests

The harness unit tests above; full ladder green.

## Risks and rollback

None beyond path updates; all three drivers already run under `make test`.
