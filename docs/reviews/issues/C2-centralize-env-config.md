# C2 · Centralize `VIBEOS_*` environment handling in the harness

| | |
|---|---|
| **Area** | 4.5 Configuration |
| **Impact / Effort / Phase** | Low / S / I |
| **Depends on** | — (do with T2) |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.5](../ARCHITECTURE_REVIEW.md#45-configuration--secrets-management) |
| **Status** | Implemented |

## Problem

`VIBEOS_ISO`, `VIBEOS_SMP`, `VIBEOS_QEMU_CPU`, `VIBEOS_MEM`, `VIBEOS_BIOS`, `VIBEOS_QEMU_ACCEL`, `VIBEOS_TIMEOUT`, `VIBEOS_QEMU_EXTRA`, `VIBEOS_EXPECT_PANIC`, `VIBEOS_GP_TEST`, `VIBEOS_EXPECT_PIT`, `VIBEOS_SKIP_PERSIST`, `VIBEOS_CRASH_ROUNDS`, `VIBEOS_CRASH_SEED`, `VIBEOS_MKFS`, `VIBEOS_FSCK` are each parsed with their own defaults in `Makefile`, `tests/harness/run_e2e.py`, `tests/kernel_boot.py`, and `tests/vibefs_crash.py`. Defaults already differ (e2e timeout 60 s, ktest and crash 90 s); `vibefs_crash.py` reads `VIBEOS_QEMU_ACCEL` itself while the others go through `harness._accel_name`.

## Recommended fix

One reader in `harness.py`, one documented table.

## Implementation plan

1. **`harness.py`:** add
   ```python
   @dataclass class EnvConfig: iso: str; smp: int; cpu: str; mem: str; bios: str | None; accel: str | None; timeout: float; extra: tuple[str, ...]
   def env_config(*, default_iso: str, default_timeout: float) -> EnvConfig
   def env_flag(name: str) -> bool     # "" and "0" are false
   def env_int(name: str, default: int) -> int
   ```
   and a module docstring table listing every `VIBEOS_*` variable, its default, and which driver honours it.
2. **Drivers:** `run_e2e.py`, `run_ktest.py` (T2's rename of `kernel_boot.py`), `run_vibefs_crash.py`, `run_ps2.py` build their `QemuConfig` from `env_config(...)`.
3. **Makefile:** keep the `VIBEOS_*` `?=` defaults as the single source for `make run`; the harness defaults must equal them (a harness unit test asserts `env_config()` defaults against values parsed from the Makefile, or simply hard-codes the same numbers with a comment).
4. **DESIGN §8.4:** replace the prose about overrides with the table.

## Acceptance criteria

- `grep -rn 'os.environ' tests | grep -v harness.py` is empty.
- Harness unit tests cover `env_flag`, `env_int`, and default equality.

## Tests

`test_harness.py` additions.

## Risks and rollback

None.
