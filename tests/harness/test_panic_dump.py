"""Unit tests for tests/harness/panic_dump.py (ROADMAP §10.7)."""

from __future__ import annotations

import unittest

from tests.harness import panic_dump
from tests.harness.frame import FRAME
from tests.harness.harness import HarnessError


def k(*texts: str) -> list[str]:
    """Kernel (framed) lines."""
    return [FRAME + t for t in texts]


BOOT = k("vibeOS: serial online", "vibeOS: boot: panic-nest armed")
NEST_DUMP = k(
    "vibeOS: panic:",
    "vibeOS: panic: at src/smp/per_cpu_init.rs:322:9",
    "vibeOS: panic: msg: irq nest underflow",
    "vibeOS: regs: rbp=0x1 rsp=0x2 rflags=0x2 rip=0x3 cr3=0x4",
    "vibeOS: panic: thread cpu=0 tid=0 bootstrap",
    "vibeOS: log: last 1 (0 dropped, 0 sink, 0 reentry)",
    "vibeOS: logrec: 5ms cpu0 info vibeOS: boot: panic-nest armed",
    "vibeOS: backtrace:",
    "  0xffffffff80001000 vibeos::smp::per_cpu_init::irq_nest_leave",
    "vibeOS: panic: halted",
)


EARLY = k(
    "vibeOS: serial online",
    "vibeOS: limine: base revision ok",
    "vibeOS: boot: early-fault armed",
    "vibeOS: panic: early vec=0x6 err=0x0 pc=0xffffffff80012345 addr=0x0 sp=0xffff800000fff000",
    "vibeOS: panic: halted",
)


class TestCheckEarly(unittest.TestCase):
    def test_good_line_passes(self) -> None:
        panic_dump.check_early(EARLY, "x86_64")

    def test_wrong_vector_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, "vec=0x6, expected 0x0"):
            panic_dump.check_early(EARLY, "aarch64")

    def test_missing_line_fails(self) -> None:
        bad = [ln for ln in EARLY if "early vec" not in ln]
        with self.assertRaisesRegex(HarnessError, "0 early-handler lines"):
            panic_dump.check_early(bad, "x86_64")

    def test_line_before_armed_does_not_count(self) -> None:
        bad = [EARLY[0], EARLY[3], EARLY[1], EARLY[2], EARLY[4]]
        with self.assertRaisesRegex(HarnessError, "0 early-handler lines"):
            panic_dump.check_early(bad, "x86_64")

    def test_full_dump_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, "2 dump banners"):
            panic_dump.check_early(EARLY[:4] + k("vibeOS: panic:") + EARLY[4:], "x86_64")


OVF_ARMED = (
    "vibeOS: boot: kstack-overflow armed guard=0xffffd000004a4000 base=0xffffd000004a8000 "
    "thread=0xffffc0000016abf0"
)
OVF_X86 = k(
    "vibeOS: serial online",
    OVF_ARMED,
    "vibeOS: panic: stack overflow far=0xffffd000004a7d78 esr=0x0 elr=0xffffffff8005cf90 "
    "sp=0xffffd000004a7d80 thread=0xffffc0000016abf0",
    "vibeOS: backtrace:",
    "vibeOS: panic: halted",
)
OVF_A64 = k(
    "vibeOS: serial online",
    "vibeOS: el: 1",
    OVF_ARMED,
    "vibeOS: panic: stack overflow far=0xffffd000004a7fc0 esr=0x96000047 "
    "elr=0xffffffff80037d3c sp=0xffffd000004a7fc0 thread=0xffffc0000016abf0",
    "vibeOS: panic: halted",
)


class TestCheckOverflow(unittest.TestCase):
    def test_good_dumps_pass(self) -> None:
        panic_dump.check_overflow(OVF_X86)
        panic_dump.check_overflow(OVF_A64)

    def test_no_overflow_line_fails(self) -> None:
        bad = [ln for ln in OVF_X86 if "stack overflow" not in ln]
        with self.assertRaisesRegex(HarnessError, "0 overflow lines"):
            panic_dump.check_overflow(bad)

    def test_line_before_armed_fails(self) -> None:
        bad = [OVF_X86[0], OVF_X86[2], OVF_X86[1], *OVF_X86[3:]]
        with self.assertRaisesRegex(HarnessError, "after the armed line"):
            panic_dump.check_overflow(bad)

    def test_far_outside_guard_fails(self) -> None:
        bad = [ln.replace("far=0xffffd000004a7d78", "far=0xffffd000004a8008") for ln in OVF_X86]
        with self.assertRaisesRegex(HarnessError, "outside the guard"):
            panic_dump.check_overflow(bad)

    def test_other_thread_fails(self) -> None:
        bad = [ln.replace("sp=0xffffd000004a7d80 thread=0xffffc0000016abf0",
                          "sp=0xffffd000004a7d80 thread=0xffffc00000000001") for ln in OVF_X86]
        with self.assertRaisesRegex(HarnessError, "thread="):
            panic_dump.check_overflow(bad)

    def test_esr_not_a_data_abort_fails(self) -> None:
        bad = [ln.replace("esr=0x96000047", "esr=0x2000000") for ln in OVF_A64]
        with self.assertRaisesRegex(HarnessError, "not a current-EL data abort"):
            panic_dump.check_overflow(bad)

    def test_no_halt_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, "0 'vibeOS: panic: halted' lines"):
            panic_dump.check_overflow(OVF_X86[:-1])


class TestCheckNest(unittest.TestCase):
    def test_good_dump_passes(self) -> None:
        panic_dump.check_nest(BOOT + NEST_DUMP)

    def test_reentered_fails(self) -> None:
        bad = BOOT + NEST_DUMP[:3] + k("vibeOS: panic: reentered") + NEST_DUMP[3:]
        with self.assertRaisesRegex(HarnessError, "re-entered"):
            panic_dump.check_nest(bad)

    def test_other_message_fails(self) -> None:
        bad = [ln.replace("irq nest underflow", "something else") for ln in BOOT + NEST_DUMP]
        with self.assertRaisesRegex(HarnessError, "irq nest underflow"):
            panic_dump.check_nest(bad)

    def test_two_banners_fail(self) -> None:
        with self.assertRaisesRegex(HarnessError, "2 dump banners"):
            panic_dump.check_nest(BOOT + NEST_DUMP + k("vibeOS: panic:"))

    def test_two_halted_fail(self) -> None:
        with self.assertRaisesRegex(HarnessError, "halted"):
            panic_dump.check_nest(BOOT + NEST_DUMP + k("vibeOS: panic: halted"))

    def test_user_copy_does_not_count(self) -> None:
        # A process printing dump text unframed changes nothing.
        panic_dump.check_nest(BOOT + ["vibeOS: panic:", "vibeOS: panic: reentered"] + NEST_DUMP)

    def test_no_message_fails(self) -> None:
        bad = [ln for ln in BOOT + NEST_DUMP if "msg:" not in ln]
        with self.assertRaisesRegex(HarnessError, "msg"):
            panic_dump.check_nest(bad)


STOP_BOOT = k(
    "vibeOS: serial online",
    "vibeOS: boot: panic-stop armed",
    "vibeOS: panic_stop: line 0",
    "vibeOS: panic_stop: line 1",
    "vibeOS: panic_stop: line 2",
)


def stop_dump(owner: int = 0, cpus: dict[int, str] | None = None) -> list[str]:
    """A good panic-stop dump from `owner`, with `cpus`' report lines."""
    if cpus is None:
        cpus = {1 - owner: "stopped (panic)", 2: "stopped (poll)", 3: "stopped (poll)",
                4: "stopped (nmi)"}
    report: list[str] = []
    for c in sorted(cpus):
        report.append(f"vibeOS: panic: cpu {c} {cpus[c]}")
        if cpus[c].startswith("stopped"):
            report.append(f"vibeOS: panic: cpu {c} regs: rip=0x1 rsp=0x2 rbp=0x3 rflags=0x2")
    return k(
        "vibeOS: panic:",
        "vibeOS: panic: at src/log/panic_test.rs:150:9",
        f"vibeOS: panic: msg: panic-stop: cpu {owner}",
        "vibeOS: panic_stop: owner nmi returned",
        "vibeOS: regs: rbp=0x1 rsp=0x2 rflags=0x2 rip=0x3 cr3=0x4",
        f"vibeOS: panic: thread cpu={owner} tid=0 bootstrap",
        "vibeOS: log: last 2 (0 dropped, 0 sink, 0 reentry)",
        "vibeOS: logrec: 5ms cpu2 info vibeOS: panic_stop: line 1",
        "vibeOS: logrec: 5ms cpu2 info vibeOS: panic_stop: line 2",
        "vibeOS: backtrace:",
        "  0xffffffff80001000 __rustc::rust_begin_unwind+0x19",
        *report,
        "vibeOS: panic: halted",
    )


class TestCheckStop(unittest.TestCase):
    def test_good_dump_passes_either_owner(self) -> None:
        panic_dump.check_stop(STOP_BOOT + stop_dump(0))
        panic_dump.check_stop(STOP_BOOT + stop_dump(1))

    def test_logrec_replays_of_numbered_lines_pass(self) -> None:
        dump = stop_dump(0)
        self.assertTrue(any("logrec" in ln and "panic_stop: line" in ln for ln in dump))
        panic_dump.check_stop(STOP_BOOT + dump)

    def test_wrong_or_missing_cpu_line_fails(self) -> None:
        good = {1: "stopped (panic)", 2: "stopped (poll)", 3: "stopped (poll)", 4: "stopped (nmi)"}
        cases = {
            1: "stopped (nmi)",
            2: "stopped (ipi)",
            3: "not stopped",
            4: "stopped (poll)",
        }
        for cpu, bad in cases.items():
            with self.subTest(cpu=cpu, bad=bad):
                cpus = {**good, cpu: bad}
                with self.assertRaisesRegex(HarnessError, f"cpu {cpu}"):
                    panic_dump.check_stop(STOP_BOOT + stop_dump(0, cpus))
            with self.subTest(cpu=cpu, missing=True):
                cpus = {c: v for c, v in good.items() if c != cpu}
                with self.assertRaisesRegex(HarnessError, f"cpu {cpu} 'no line'"):
                    panic_dump.check_stop(STOP_BOOT + stop_dump(0, cpus))

    def test_extra_cpu_line_fails(self) -> None:
        cpus = {1: "stopped (panic)", 2: "stopped (poll)", 3: "stopped (poll)",
                4: "stopped (nmi)", 5: "not stopped"}
        with self.assertRaisesRegex(HarnessError, "unexpected"):
            panic_dump.check_stop(STOP_BOOT + stop_dump(0, cpus))

    def test_numbered_line_after_panic_fails(self) -> None:
        dump = stop_dump(0)
        dump.insert(3, FRAME + "vibeOS: panic_stop: line 3")
        with self.assertRaisesRegex(HarnessError, "after the first panic line"):
            panic_dump.check_stop(STOP_BOOT + dump)

    def test_two_banners_fail(self) -> None:
        dump = stop_dump(0)
        dump.insert(1, FRAME + "vibeOS: panic:")
        with self.assertRaisesRegex(HarnessError, "2 dump banners"):
            panic_dump.check_stop(STOP_BOOT + dump)

    def test_no_owner_nmi_line_fails(self) -> None:
        dump = [ln for ln in stop_dump(0) if "owner nmi" not in ln]
        with self.assertRaisesRegex(HarnessError, "owner nmi returned"):
            panic_dump.check_stop(STOP_BOOT + dump)

    def test_aarch64_good_dump_passes(self) -> None:
        boot = k(
            "vibeOS: serial online",
            "vibeOS: el: 1",
            "vibeOS: gic: v3",
            "vibeOS: boot: panic-stop armed",
            "vibeOS: panic_stop: line 0",
            "vibeOS: panic_stop: line 1",
            "vibeOS: panic_stop: line 2",
        )
        cpus = {
            1: "stopped (panic)",
            2: "stopped (poll)",
            3: "stopped (poll)",
            4: "not stopped",
        }
        dump = [ln for ln in stop_dump(0, cpus) if "owner nmi" not in ln]
        panic_dump.check_stop(boot + dump)

    def test_owner_not_zero_or_one_fails(self) -> None:
        dump = [ln.replace("thread cpu=0", "thread cpu=2") for ln in stop_dump(0)]
        with self.assertRaisesRegex(HarnessError, "owner"):
            panic_dump.check_stop(STOP_BOOT + dump)

    def test_no_frame_fails(self) -> None:
        dump = [ln for ln in stop_dump(0) if "  0x" not in ln]
        with self.assertRaisesRegex(HarnessError, "frame"):
            panic_dump.check_stop(STOP_BOOT + dump)


GP_DUMP = k(
    "vibeOS: #GP rip=0xffffffff8002b6b8 cs=0x8 rflags=0x2 rsp=0x1 ss=0x10 err=0x8",
    "vibeOS: backtrace:",
    "  0xffffffff8002b6b8 vibeos::gp_test_fault+0x8",
    "  0xffffffff8002b6a2 vibeos::gp_test_trip+0x22",
    "  0xffffffff8002bdc3 vibeos::boot_rest+0x423",
    "vibeOS: panic: halted",
)


class TestFramesInOrder(unittest.TestCase):
    def test_in_order_passes(self) -> None:
        panic_dump.check_frames_in_order(GP_DUMP, ("gp_test_trip", "boot_rest"))
        panic_dump.check_frames_in_order(GP_DUMP, ("gp_test_fault", "gp_test_trip", "boot_rest"))

    def test_reversed_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, "gp_test_trip"):
            panic_dump.check_frames_in_order(GP_DUMP, ("boot_rest", "gp_test_trip"))

    def test_missing_frame_fails(self) -> None:
        dump = [ln for ln in GP_DUMP if "gp_test_trip" not in ln]
        with self.assertRaisesRegex(HarnessError, "gp_test_trip"):
            panic_dump.check_frames_in_order(dump, ("gp_test_trip", "boot_rest"))

    def test_segment_match_only(self) -> None:
        # `gp_test_trip` does not match a frame for `xgp_test_trip`, nor a
        # logrec replay or a user copy of a frame line.
        dump = [ln.replace("vibeos::gp_test_trip", "vibeos::xgp_test_trip") for ln in GP_DUMP]
        dump += k("vibeOS: logrec: 1ms cpu0 info   0x1 vibeos::gp_test_trip")
        dump += ["  0xffffffff8002b6a2 vibeos::gp_test_trip+0x22"]
        with self.assertRaises(HarnessError):
            panic_dump.check_frames_in_order(dump, ("gp_test_trip", "boot_rest"))

    def test_symbols(self) -> None:
        self.assertEqual(
            panic_dump.frame_symbols(GP_DUMP + k("  0x1")),
            ["vibeos::gp_test_fault", "vibeos::gp_test_trip", "vibeos::boot_rest", ""],
        )


if __name__ == "__main__":
    unittest.main()
