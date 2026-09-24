# Design reviews

Decisions of the design reviews of the roadmap and the design documents. The kernel review's findings
are in [KERNEL_REVIEW.md](KERNEL_REVIEW.md), and the architecture and roadmap reviews are
[ARCHITECTURE_REVIEW.md](ARCHITECTURE_REVIEW.md) and [ROADMAP_REVIEW.md](ROADMAP_REVIEW.md). A design
review writes each decision into the document it changes, with its reasons and the alternatives it
rejected; this file indexes them, so an id cited in `docs/` resolves after a squash merge folds a
review's commits into one.

A row gives the id, its severity (Blocker, Major, Minor, or Editorial; H015 and H016, which the second
review put to the owner, carry its Legal and Privacy classes instead), the decision in one line, the
sections it changed, and the pull request. An owner decision also gives the owner's answer and its
date. From ROADMAP §10.9's box, `scripts/check_review_refs.py` fails on a cited id with no row here.

## G (2026-09-23)

| Id | Severity | Decision | Sections | PR |
|----|----------|----------|----------|----|
| G001 | Blocker | The kernel is preemptible wherever IF=1: IF=0 only in listed, bounded contexts, and syscall and CPL-3 fault bodies run with IF=1 | DESIGN §2.9, §5.10; ROADMAP §10.3, §10.6; SYSCALL §1; AGENTS rule 2 | #92 |
| G002 | Major | The physmap base is Limine's HHDM offset, read once into `BootInfo`; an overlap with a fixed region halts with a named reason | DESIGN §4.1; ROADMAP §11.1 | #92 |
| G003 | Blocker | Two allocation-failure policies: fallible, failing with `ENOMEM`, where untrusted input reaches; infallible only at boot or where an invariant bounds it, and there a failure panics | DESIGN §4.4; ROADMAP §10.4, §12.6; AGENTS rule 4 | #92 |
| G004 | Blocker | Sleeping locks form a tier outside the spin ranks, taken only with IF=1 and no spinlock held: namespace and inode, address space, page and buffer, then block mapping and volume | DESIGN §2.1; ROADMAP §13.1, §13.12; issue plan A3 | #92 |
| G005 | Major | Each architecture concern is a trait in `vibeos-core`, implemented on one zero-sized port type and taken as a type parameter, never `dyn` | DESIGN §11.1; ROADMAP §10.3 | #92 |
| G006 | Major | A table of trust boundaries: each principal, what it is trusted for, what it can do today, and the line that hardens it. Owner: (a), accepted until Phase 18, 2026-09-23 (DESIGN §2.10) | DESIGN §2.10; ROADMAP §18.8; README | #92 |
| G007 | Major | vibefs v2's limits and requirements are pinned wide now | VIBEFS §15; ROADMAP §14.8 | #92 |
| G008 | Minor | vibefs v1 has no compatibility promise: its kernel code is deleted once root is v2, and nothing converts a v1 volume | ROADMAP §14.8, §18.5; VIBEFS header, §14 | #92 |
| G009 | Major | Five object-lifetime rules; pids and tids are allocated in increasing order up to `pid_max`, then wrap | DESIGN §2.11; ROADMAP §10.4 | #92 |
| G010 | Major | Phase 10 lands in three waves: the live CRITICAL and HIGH boxes, then the file moves, then the rest | ROADMAP Phase 10 | #92 |
| G011 | Minor | An exit-gate line is never deferred past its own phase's tag, Phases 8 to 14 are tagged in order, and `[0.8.0]` stays undated | ROADMAP How to read this, standing gates; README; CHANGELOG | #92 |
| G012 | Editorial | DESIGN §1.4 names the "Rule; not yet enforced" note as the one sanctioned status | DESIGN §1.4 | #92 |
| G013 | Editorial | DESIGN §2.3 counts two cells and a lock, as AGENTS does | DESIGN §2.3 | #92 |
| G014 | Editorial | DESIGN §8.6 says which CI rows run on every push | DESIGN §8.6 | #92 |
| G015 | Major | No native syscall numbers: a vibeOS interface is a file, an ioctl, or a generic-netlink family | SYSCALL §8; ROADMAP How to read this, §13.9 | #92 |
| G016 | Major | "Linux's" means a baseline release named in `docs/LINUX.md`, and `uname` reports sysname Linux | ROADMAP How to read this, §13.10 | #92 |
| G017 | Major | From §11.2 the physmap maps only RAM-typed ranges on both architectures, and MMIO goes through `ioremap` | DESIGN §4.1, §4.3; ROADMAP §11.2, §27.3 | #92 |
| G018 | Minor | `vibeos-core` builds on stable Rust, which `check_core_stable.py` checks | DESIGN §1.1; ROADMAP §10.1, §38.1 | #92 |
| G019 | Minor | Goals win in a ranked order: no halt, corruption, or data loss from untrusted input; Linux's contract; host-side explainability; one mechanism over two; measured speed | DESIGN §1.1 | #92 |
| G020 | Editorial | Cross-references cite ROADMAP sections, invariant ids are kept apart from the architecture review's issue ids, and §10.6 and §13.9 take follow-on fixes | DESIGN; ROADMAP §10.6, §13.9 | #92 |
| G021 | Major | `free_vector` returns only when no handler for the vector is running or queued; teardown stops the device, frees its vectors, then frees its state | DESIGN §5.4; ROADMAP §20.9 | #92 |
| G022 | Minor | The VFS takes Linux's `NAME_MAX` of 255 and `PATH_MAX` of 4096, with path buffers allocated fallibly | SYSCALL §2; ROADMAP §13.9 | #92 |
| G023 | Major | The display protocol is Wayland, with bindings generated from the pinned XML, and vibeOS's own needs are Wayland extensions | ROADMAP §16.5, §36.2, §39.1 | #92 |
| G024 | Minor | The Rust target triple is `<arch>-unknown-linux-musl`, and upstream std is used unpatched | ROADMAP §24.3, §24.6 | #92 |
| G025 | Major | `BootInfo` and the handover table share one versioned format of tagged records, and an unknown required tag is refused before memory is touched | ROADMAP §25.4 | #92 |
| G026 | Major | A one-time Verus spike over the real allocator in §12.1, recorded in `docs/VERIFIED.md` | ROADMAP §12.1, §38.1 | #92 |
| G027 | Major | An operation makes every allocation before its point of no return | DESIGN §4.4; VIBEFS §10, §15; ROADMAP §10.4, §14.8 | #92 |
| G028 | Major | A superblock write that reports an error may have landed: the failed commit keeps its blocks, and the retry writes the same slot | VIBEFS §10; ROADMAP §12.5, §38.3 | #92 |
| G042 | Major | Nothing is copied or translated from GPL or LGPL sources, and interface constants and layouts are cited as facts | DESIGN §1.5; AGENTS | #92 |
| G043 | Minor | A dev-host record names the machine as `dev-host` plus the Mac model, never a hostname, user name, or home path | ROADMAP §10.9 | #92 |

Ids G029 to G041 are unused: G042 and G043 were numbered apart as the legal and privacy items.

## H (2026-09-23)

| Id | Severity | Decision | Sections | PR |
|----|----------|----------|----------|----|
| H001 | Blocker | Direct reclaim writes no page and takes no sleeping lock; it unmaps under the page-table spinlock, and the address-space lock guards regions only | DESIGN §2.1, §2.2, §4.4; ROADMAP §12.1, §12.5, §12.6, §12.7, §13.1, §19.10 | #92 |
| H002 | Major | A rename locks an ancestor before its descendant, and address order only for unrelated directories; no user copy runs under a level-2, 3, or 4 lock | DESIGN §2.1; ROADMAP §13.12 | #92 |
| H003 | Major | One bottom-half thread per threaded vector, pinned to the vector's CPU | DESIGN §5.4; ROADMAP §12.5, §20.9 | #92 |
| H004 | Major | Every block request has a deadline, 30 s by default, and the timeout aborts or resets the device before it fails the request | DESIGN §10.3; ROADMAP §12.5, §29.1 | #92 |
| H005 | Major | vibefs v2 keeps one CRC-32C per data block, in a checksum tree keyed by physical block number | VIBEFS §15 | #92 |
| H006 | Major | A box is ticked only by the commit that makes its proving test pass, with a `Proves:` trailer per box that `check_ticks.py` enforces | ROADMAP How to read this, standing gates, §10.9; AGENTS; PR template | #92 |
| H007 | Major | Signatures name a key id, images trust a key set, and a root-signed key-set record that only moves forward adds and revokes keys. Owner: (b), "for now", 2026-09-23 (ROADMAP §14.6) | ROADMAP §14.6, §18.7, §22.4, §39.1 | #92 |
| H008 | Minor | Fallible allocation goes through `vibeos::kalloc`'s types, and `disallowed-types` denies `alloc`'s owning types | DESIGN §4.4; ROADMAP §10.4; AGENTS rule 4 | #92 |
| H009 | Minor | §12.6 sizes the kernel heap region from installed memory | DESIGN §4.1; ROADMAP §12.6, §27.3 | #92 |
| H010 | Minor | The early IDT, and an early aarch64 vector table, land in §11.1 | ROADMAP §11.1, §20.1 | #92 |
| H011 | Minor | aarch64 saves user FP state at each switch away from a thread whose state is live, as x86_64 does | ROADMAP §11.1, §11.6, §13.8, §23.1 | #92 |
| H012 | Minor | User mappings follow Linux's top-down layout from `mmap_base`, with its legacy switch | ROADMAP §12.4 | #92 |
| H013 | Minor | vibefs v2 stores extended attributes under any name; file capabilities land in §18.6 and §21.5, and POSIX ACLs are a listed gap | VIBEFS §15; ROADMAP §14.8, §18.6, §21.5 | #92 |
| H014 | Minor | Workflows declare least-privilege token permissions, and `check_workflows.py` requires a `permissions:` block | ROADMAP §10.1; `ci.yml`, `smp-stress.yml` | #92 |
| H015 | Legal | The licenses a release image may carry are the owner's decision. Owner: (b), 2026-09-23 (ROADMAP §14.10) | ROADMAP §14.10 | #92 |
| H016 | Privacy | What a crash report sends, and where, is the owner's decision. Owner: nothing is posted, reopened before any report leaves the machine, 2026-09-23 (ROADMAP §37.4) | ROADMAP §37.4 | #92 |
| H017 | Editorial | The monolithic decision's rationale is written down, and the harness's `boot_contract_markers()` is the executable boot contract | DESIGN §1, §3.3, §8.3 | #92 |
| H018 | Minor | vibefs v2's `fsync` is measured against ext4 when v2 lands, and an intent log arrives in §25.7 if its p50 is above twice ext4's | VIBEFS §15; ROADMAP §14.8, §25.7, §38.3 | #92 |

## J (2026-09-24)

| Id | Severity | Decision | Sections | PR |
|----|----------|----------|----------|----|
| J001 | Major | Land each LATENT fix before the Phase 10 box that makes it reachable | ROADMAP Phase 10, §10.3, §10.4, §10.6, §13.1, §13.8; SYSCALL §4 | #92 |
| J002 | Major | Put the agent boundary to the owner: own GitHub identity, root key away from agents | ROADMAP How to read this, §14.6, Era VI. Production, Phase 30, Funded goals, Paid services; DESIGN §2.10; AGENTS | #92 |
| J003 | Major | Make the address space a two-count object in §10.6 and fix its teardown order | ROADMAP Phase 10, §10.6, §12.1, §12.3, §12.6, §13.1, §13.9, §17.4, §23.4; DESIGN §2.1, §2.3, §2.7, §2.11, §4.4 | #92 |
| J004 | Major | An object reverse map with its own sleeping lock and a walk order fork and mremap keep | ROADMAP §12.1, §12.3, §12.4, §12.6, §12.7, §13.5, §13.12, §19.10, Phase 38; DESIGN §2.1, §4.4, §4.6 | #92 |
| J005 | Major | Name §2.11's count types, split teardown into last put and kill, and defer releases from atomic context | ROADMAP §10.4, §12.5, §13.7, §20.9, §23.1; DESIGN §2.2, §2.11, §10.1 | #92 |
| J006 | Major | Keep data I/O off the VFS lock and give each volume a BlockingMutex in §10.4 | ROADMAP Phase 8, §10.4, §12.5, §13.8, §13.9; DESIGN §2.1, §7.7; SYSCALL §2; issue plans A3 | #92 |
| J007 | Major | IST vectors taken at CPL 3 swap GS by CS.RPL; only CPL-0 frames and #DF read GS_BASE | ROADMAP §10.6, Phase 18, §18.3, §25.1; DESIGN §2.2, §2.7, §5.10, §7.5, §9.3; AGENTS | #92 |
| J008 | Major | One FP binding on both architectures, with no FP access trap and one initial image | ROADMAP §3.2, §9.1, §10.6, §11.1, §11.6, §13.10, §19.3, §20.9, §23.1; DESIGN §7.5 | #92 |
| J009 | Major | After boot every allocation is fallible, and exit allocates nothing past its first release | ROADMAP §10.4, §12.6; DESIGN §2.2, §2.5, §3.3, §4.4; AGENTS | #92 |
| J010 | Major | Block completion runs in two stages, and a bottom half never waits for an I/O completion | ROADMAP §12.5, §14.8, §18.7, §19.8, §29.1, §29.4; DESIGN §2.2, §4.4, §5.4, §10.1; VIBEFS §15 | #92 |
| J011 | Major | Add a §12 device model with counted devices, a device tree, and resources the probe claims | ROADMAP §6.1, §10.4, §10.12, §11.5, §18.1, §20.2, §20.3, §20.7, §20.9, §22.2, §23.3, §25.4, §29.1; DESIGN Contents, §10.6, §11.4, §12, §12.1, §12.2, §12.3; issue plans D2 | #92 |
| J012 | Major | v2 pointers carry the child's generation and checksum, so a lost write is caught | ROADMAP §14.8, §29.4, §38.3; VIBEFS §5, §11, §15 | #92 |
| J013 | Major | Block headers carry a meta_uuid, the user-visible uuid changes in two writes, and a two-device lookup is refused | ROADMAP §14.8, Phase 26, §26.2, §26.6, §29.1; DESIGN §10.5; VIBEFS §15 | #92 |
| J014 | Major | v2 snapshots track space by birth generation with deadlists, and clones become a later feature | ROADMAP §14.8, §18.5, §23.1, Beyond; DESIGN §2.7; VIBEFS §2, §11, §15 | #92 |
| J015 | Major | Completion waits, Flush and FUA replace Barrier, and a failed reset clears bus mastering | ROADMAP §7.1, §10.11, §29.1, §38.3; DESIGN Contents, §2.7, §10.2, §10.3, §10.4, §10.6; VIBEFS header, §2, §10 | #92 |
| J016 | Major | The intent-log trigger measures PostgreSQL's commit pattern, and the log is a compat feature with an incompat replay bit | ROADMAP §14.8, §25.7, Era VIII. Assurance, §38.3; VIBEFS §15 | #92 |
| J017 | Major | Native interfaces live under vibeOS names, and the syscall table is checked against the baseline's own numbers | ROADMAP How to read this, §9.3, §10.2, §10.7, §10.11, §12.2, §12.3, §13.8, §13.9, §20.1, §25.7, §29.1, §32.1, §32.2, §33.2; DESIGN §3.2; SYSCALL §8; LINUX.md Native interfaces; `scripts/check_linux_md.py`, `tests/harness/test_linux_md.py` | #92 |
| J018 | Major | One oracle kernel per architecture, built from kernel.org, is what every Linux comparison boots | ROADMAP How to read this, The arc, §12.4, Phase 13, §13.10, §13.11, §14.8, §14.9, §14.10, Phase 18, §21.3, §23.1, §23.6, Era VI. Production, Era VII. Daily Driver, Phase 31, §31.2, §31.7, §32.2, Phase 33, §33.2, §33.3, Phase 34, §35.4, Phase 36, Phase 37, §37.2, §37.3; DESIGN §1.5; SYSCALL header; LINUX.md Baseline, Deliberate differences | #92 |
| J019 | Major | One seam table, pure port halves in vibeos-core, one machine description | ROADMAP §10.1, §10.2, §10.3, Phase 11, §11.3, §11.4, §11.5, §11.8, §19.6, §20.7; DESIGN Contents, §7.1, §8.1, §11, §11.1, §11.3; issue plans A1, Q4 | #92 |
| J020 | Major | The booted image draws its own address map through one direct entry per architecture, and kexec only places bytes | ROADMAP §18.2, Era VI. Production, §25.4, §26.4, Bare metal and hardware CI; DESIGN §3.3, §4.1, §11.1 | #92 |
| J021 | Major | Resolve paths only in the Vfs walker and hold each process's cwd and root as counted references | ROADMAP §10.4, §13.9, §13.10, §20.1; DESIGN §2.11; issue plans A3 | #92 |
| J022 | Major | Sign releases in a key job that runs no candidate code, dispatched from main | ROADMAP standing gates, §10.1, §10.7, §10.9, §14.6, Phase 22, §22.1, §22.4, Phase 24, §24.2, Phase 39, §39.3, Public clouds; DESIGN §8.6; issue plans B3 | #92 |
| J023 | Major | Prove vibefs v2's read-time validation in §14.8, not §18.5 | ROADMAP Phase 14, §14.8, §18.5; VIBEFS header, §5, §11, §15 | #92 |
| J024 | Major | Phase 10 order as a needs file; wave 1 closed under its needs, wave 2 only moves | ROADMAP Phase 10, §10.4, §10.9; AGENTS; PR template; `tests/gates/phase-10-needs.toml`; issue plans D2, Q5 | #92 |
| J025 | Minor | A slice is a series of PRs, not one PR | ROADMAP How to read this | #92 |
| J026 | Major | State what Secure Boot covers and fit A/B slots, kexec, and PVH to it | ROADMAP §18.7, §22.2, §25.4, §26.4, Funded goals; DESIGN §2.10, §3.2 | #92 |
| J027 | Major | Ask the owner what H016 covers, pin the remote vmcore's TLS, and cap local cores | ROADMAP §25.4, §25.6, §30.3, §37.4 | #92 |
| J028 | Minor | List every connection a shipped image makes on its own, and put defaults to the owner | ROADMAP §22.2, §30.5, §35.3, §37.2 | #92 |
| J029 | Minor | State what the project's CI and machines publish, and ask the owner whether H016 reaches them | ROADMAP §10.7, §10.9, §37.4, §39.3, Bare metal and hardware CI, Daily-driver hardware; DESIGN §1.5 | #92 |
| J030 | Major | Ship third-party notices on every image, and ask the owner where copyleft sources live | ROADMAP §10.9, §14.10, §22.1; DESIGN §1.5 | #92 |
| J031 | Minor | daily-driver lines run a test image until the owner answers the firmware question | ROADMAP §31.6, Daily-driver hardware | #92 |
| J032 | Major | Rank the heap first, so any allocation under PT or BUDDY fails the check | ROADMAP §10.3; DESIGN §2.1, §2.7, §4.4 | #92 |
| J033 | Major | State the spin contract, lock-free work inside IF=0 sections, and nested same-rank locks | ROADMAP §10.3, §13.12, §25.5; DESIGN §2.1, §2.2, §2.3, §2.7, §2.9, §5.10, §7.6, §7.9, §9.4 | #92 |
| J034 | Major | One panic stop primitive on both architectures, polled in every serviced spin | ROADMAP §10.7, §11.3, §11.7, Phase 25, §25.4, §25.5; DESIGN §2.3, §2.5, §5.2, §5.3, §7.6, §8.3; issue plans A4 | #92 |
| J035 | Major | `current` is read in one instruction that preemption cannot split | ROADMAP §10.3, §11.4, §11.6, §19.4; DESIGN §2.7, §2.9, §7.5, §11.1 | #92 |
| J036 | Major | Give Phase 13's position, stream and TTY locks their order | ROADMAP §13.1, §13.2, §13.3, §13.5, §13.7, §13.12; DESIGN §2.1; SYSCALL §4 | #92 |
| J037 | Minor | Drop region references and write back after the address-space lock | ROADMAP §12.4, §13.1, §13.12; DESIGN §2.1, §4.4 | #92 |
| J038 | Major | Give network receive, sockets and timer callbacks a context and rank | ROADMAP §13.3, §13.9, §13.12, §15.1, §15.5, §15.6, §15.10; DESIGN §2.1, §2.2, §6.5 | #92 |
| J039 | Major | Bound IF=0 stretches by an instruction count a nightly run checks | ROADMAP §10.3, §10.6, §12.1, §12.3, §12.4, §12.6, §19.3, §19.9; DESIGN §2.7, §2.9, §4.4; AGENTS | #92 |
| J040 | Major | Entry stubs save CR2, DR6, ESR and FAR before IF can turn on | ROADMAP §10.6, §11.3, §12.2, §25.5; DESIGN §2.7, §2.9, §5.2, §5.10; AGENTS | #92 |
| J041 | Major | The IPI send orders the stores it publishes, on xAPIC, x2APIC and GICv3 | ROADMAP §11.3, §20.1; DESIGN §7.3, §7.4, §7.6, §7.10, §9.5 | #92 |
| J042 | Major | A tracer or core dump touches a thread's saved state only once its CPU has switched away | ROADMAP §10.10, §13.8, §17.4; DESIGN §2.11, §7.5 | #92 |
| J043 | Major | RCU is preemptible, and counted objects wait for a grace period after their last put | ROADMAP §15.4, §19.4, §19.5, §28.5, §38.3; DESIGN Contents, §2.2, §2.9, §2.11, §2.12, §4.4 | #92 |
| J044 | Major | Two timer structures per CPU, with wakes expiring in the timer interrupt | ROADMAP §13.6, §13.9, §19.4, §19.6, §28.5; DESIGN Contents, §2.1, §2.2, §6.4, §6.5, §6.6, §7.7, §7.8 | #92 |
| J045 | Major | now_ns reads one clocksource chosen at boot and never counts ticks | ROADMAP §10.1, §10.3, §11.3, §13.10, §19.1, §19.3, §19.6, §20.2, §21.4, §26.5, §30.5; DESIGN §6.1, §6.4, §6.6, §9.4, §11.1; `tests/gates/phase-10-needs.toml` | #92 |
| J046 | Major | Give kernel threads Linux's scheduling classes and budget network receive | ROADMAP §15.1, §15.2, §19.4, §25.5, §28.2; DESIGN §2.2, §5.4, §7.8 | #92 |
| J047 | Major | Specify CPU offline and online as a registry, a rendezvous and a parked state | ROADMAP Phase 19, §19.4, §19.5, §19.6, §19.9, §20.2, §23.3, §25.1, §27.3, §28.1, §28.5; DESIGN Contents, §2.9, §7, §7.8, §7.9, §7.10, §7.11 | #92 |
| J048 | Major | SpinMutex becomes one queued lock, and a halted waiter still services IPIs | ROADMAP §21.4, Phase 27, §27.5, Public clouds; DESIGN §2.3 | #92 |
| J049 | Major | A move-only Frames token and one count per present PTE on per-unit metadata | ROADMAP §10.3, §12.1, §12.2, §12.3, §19.8, §23.4, §27.4, Phase 38, §38.3; DESIGN §4.2, §4.5, §4.6, §4.7 | #92 |
| J050 | Major | One TLB rule for every translation, and dirty bits folded before a page is clean | ROADMAP §12.2, §12.3, §12.4, §12.6, §13.1, §18.1, §27.4; DESIGN §2.4, §2.7, §4.3, §4.4, §4.5, §7.9; `src/kva.rs` | #92 |
| J051 | Major | Shootdown targeting lands once in §12.3, with PCID generations and a lazy-TLB table exception | ROADMAP §10.3, §11.2, §11.6, §12.3, §18.3, §27.5, §38.3; DESIGN §2.9, §7.5, §7.9 | #92 |
| J052 | Major | One page cache of mappings, keyed by index and never by device location | ROADMAP §12.5, §19.5; DESIGN §2.1, §4.6, §10.6; VIBEFS §6, §10; `src/cache.rs`, `src/cache_init.rs` | #92 |
| J053 | Major | Faults recheck the PTE they read, truncate unmaps, and past EOF is SIGBUS | ROADMAP §12.2, §12.3, §12.4; DESIGN §5.2 | #92 |
| J054 | Major | The fault path drops the address-space lock to wait, and a fatal signal ends its waits | ROADMAP §12.6, §12.7, §13.1, §13.8, §13.12, §17.4, §19.3; DESIGN §2.1 | #92 |
| J055 | Major | Freeing memory needs no memory, and the TLB gather flushes every 64 units | ROADMAP §10.4, §12.1, §12.3, §12.4, §12.6, §19.3; DESIGN §4.3, §4.4 | #92 |
| J056 | Major | The first store to a clean shared file page faults to reserve space, dirty it and update mtime | ROADMAP §12.2, §12.4, §12.5, §14.8; DESIGN §4.3, §4.4, §5.2; VIBEFS §10, §13, §15 | #92 |
| J057 | Major | One reserve reached by class depth, and socket memory bounded in Phase 15 | ROADMAP §12.5, §12.6, Phase 15, §15.1, §15.3, §15.5, §15.6, §15.10, §19.5, §19.10, §23.4, §28.5; DESIGN §2.2, §4.4, §5.4 | #92 |
| J058 | Major | Direct reclaim waits for writeback progress before the OOM killer runs, and commits have a memory bound | ROADMAP §12.5, §12.6, §14.8, §19.10; DESIGN §2.1, §4.4; VIBEFS §15 | #92 |
| J059 | Major | Keep receive, refill and TCP-timer obligations when an atomic allocation fails | ROADMAP Phase 15, §15.2, §15.6, §15.10, §28.5; DESIGN §4.4 | #92 |
| J060 | Major | The OOM killer spares pid 1, reaps its victim, never panics; overcommit | ROADMAP Phase 12, §12.4, §12.6, §19.10, §21.5, §23.4; DESIGN §4.4 | #92 |
| J061 | Major | A second-level translation of guest memory is invalidated before its frame is freed | ROADMAP §12.3, §21.1, §21.2, §27.4; DESIGN §2.4, §2.7 | #92 |
| J062 | Major | Hot removal kills devices in §2.11's order and states what mounts, mappings and netdevs see | ROADMAP §15.1, §18.1, Phase 20, §20.3, §20.9, §21.7; DESIGN Contents, §5.4, §10.3, §12.3, §12.4; VIBEFS §10 | #92 |
| J063 | Major | A3 moves F013's FAT inode into Vfs, keyed by its dirent location | ROADMAP §10.4; issue plans A3 | #92 |
| J064 | Major | Separate mounts from superblocks, check umount per mount, and land MNT_DETACH and MS_BIND before their callers | ROADMAP §8.1, §10.4, §13.9, §18.6, §23.5; DESIGN §2.11; issue plans A3 | #92 |
| J065 | Minor | 128 MiB memory sections that join the buddy before the reserve and reclaim, never from an atomic context | ROADMAP §12.1, §27.3, §27.6; DESIGN §4.2, §4.4 | #92 |
| J066 | Major | Verify a data block before it enters the page cache, and carry F063's tests to v2 | ROADMAP §12.5, §14.8, §29.4; DESIGN §10.6; VIBEFS §9, §15 | #92 |
| J067 | Major | Every user entry saves one user frame, and sysretq runs only when Linux's test passes | ROADMAP §10.6, §11.6, §13.8, §13.10, §17.4; DESIGN §2.7, §2.9, §5.10, §7.5, §11.1; SYSCALL §1 | #92 |
| J068 | Major | One return-state validator per architecture; exit work checked with IF=0 | ROADMAP §10.6, §11.2, §12.6, §13.8, §17.4; DESIGN §2.7, §2.9, §5.10, §11.2; AGENTS | #92 |
| J069 | Major | aarch64 rows in §5.10 and §7.5, a new §11.5, and TLS switching in §11.6 | ROADMAP §9.4, §11.3, §11.5, §11.6, §13.1; DESIGN Contents, §2.7, §2.9, §5, §5.10, §6, §7, §7.5, §11.1, §11.5; SYSCALL header; AGENTS | #92 |
| J070 | Major | Traps decode to a portable TrapKind, one ring-3 table for both ports | ROADMAP §10.6, §11.3, §11.6, Phase 12, §28.4; DESIGN §2.5, §2.7, §5.2, §5.10, §9.3, §11.1, §11.5; AGENTS | #92 |
| J071 | Major | aarch64 reports kernel stack overflow from a per-CPU overflow stack | ROADMAP Phase 11, §11.3, §11.7; DESIGN §2.2, §4.3, §4.5, §5.10, §7.5, §9.2, §11.1, §11.5; AGENTS | #92 |
| J072 | Minor | Count and ignore an interrupt no handler owns; the vector table derives its error-code flags | ROADMAP §10.3, §10.6; DESIGN §2.10, §3.3, §5.2, §5.4, §5.5 | #92 |
| J073 | Major | Give debug registers one owner per build and decode tracer writes as Linux does | ROADMAP §11.4, §17.4, Phase 18, §18.4; DESIGN §2.5, §5.2, §7.5 | #92 |
| J074 | Major | signal-handler entry state, a forced SIGSEGV on failed delivery, and per-thread fault siginfo | ROADMAP §13.8, §17.4; DESIGN §5.10 | #92 |
| J075 | Major | Every control user code sees is written whole at bring-up and tabulated | ROADMAP §11.4, §11.6, §13.10, §13.11, §23.1; DESIGN Contents, §7.5, §11.3, §11.4; AGENTS | #92 |
| J076 | Major | Every unprivileged interface names Linux's check and bound, with a uid 1000 test | ROADMAP standing gates, §13.3, §13.6, §13.8, §13.9, §14.8, §18.5, §18.6, §18.8, §19.1, §19.2, §19.4, §21.2, §21.5, §23.1, §23.4, §23.5, §31.2, §34.4, §36.1, §36.2 | #92 |
| J077 | Major | A measured kernel stack budget replaces 'raise it if tight' | ROADMAP §8.6, §10.2, §10.4, §13.9; DESIGN §2.2, §3.5, §4.5 | #92 |
| J078 | Major | Create docs/LINUX.md with checked tables, and give each unlisted Linux divergence a row or a fixing line | ROADMAP How to read this, §10.4, §10.5, §10.6, §12.5, §13.7, §13.11; DESIGN Contents, §1.4, §2.11, §4.4, §5.2, §11.4; SYSCALL §2, §3; LINUX.md header, Baseline, Deliberate differences, Native interfaces; README; `scripts/check_linux_md.py`, `tests/harness/test_linux_md.py` | #92 |
| J079 | Major | User selectors take Linux's 0x33 and 0x2b, data selectors are null and per thread, and rt_sigreturn checks CS and SS as Linux does | ROADMAP §10.6, §13.8, §13.11; DESIGN §5.1, §7.2, §7.5; SYSCALL §1; LINUX.md Deliberate differences | #92 |
| J080 | Major | The argument contract follows Linux's: widths, error range, check order, partial copies, flags, and extensible structs | ROADMAP §9.3, §10.5, §10.6, §11.6, §13.9, §13.10, §13.11, §13.13, §23.1; SYSCALL header, §1, §2, §3, §5 | #92 |
| J081 | Major | Futex takes the bitset operations and flags Rust's std and glibc use | ROADMAP §13.5, §13.9, §13.11, §13.12; DESIGN §6.6 | #92 |
| J082 | Minor | Version the Linux surface by its baseline and re-freeze the syscall table at every release | ROADMAP How to read this, §22.1, Phase 39, §39.1, §39.2; LINUX.md header, Baseline log | #92 |
| J083 | Major | KVM and VFIO discovery reports what vibeOS implements, and Linux's KVM tests gate Phase 21 | ROADMAP How to read this, Phase 21, §21.2, §28.4 | #92 |
| J084 | Major | The user runtime builds for the musl triples as no_std, never for the kernel's soft-float targets | ROADMAP §10.5, §11.1, §11.6, §13.10, §17.3, §17.7, §24.3; DESIGN §3.1, §11.4 | #92 |
| J085 | Major | aarch64 ASIDs come from a generation allocator that reserves running ASIDs and flushes each CPU locally at rollover | ROADMAP §11.2, §11.7, §38.5; DESIGN §2.7, §7.5, §11.1, §11.2 | #92 |
| J086 | Major | Drivers name interrupts by IrqId, and each interrupt controller is an IrqChip object, landing on x86_64 before the GIC | ROADMAP §6.3, §11.3, §11.5, §25.4, §27.1; DESIGN §5.3, §5.4, §5.9, §11.1 | #92 |
| J087 | Major | MMIO accessors order like writel and readl, and DMA coherence is per device, from firmware | ROADMAP §11.2, §11.5, §11.7, §20.7; DESIGN §2.4, §4.7, §9.3, §11.1 | #92 |
| J088 | Minor | The aarch64 kernel requires FEAT_LSE and FEAT_PAN and checks them at boot; DESIGN records both ISA floors | ROADMAP §11.1, §11.6, §11.7, §13.10; DESIGN §3.1, §11.1, §11.4 | #92 |
| J089 | Major | Class every interface stable, unstable, internal, or deprecated over a generated inventory | ROADMAP §10.2, §13.9, Phase 39, §39.1; DESIGN §2.6, §3.2; LINUX.md Native interfaces | #92 |
| J090 | Major | v2 superblocks sit at fixed byte offsets, and mount falls back one generation over deferred frees | ROADMAP §14.8, §26.2, §29.5, §38.3; DESIGN §2.7; VIBEFS §2, §15 | #92 |
| J091 | Major | v2 requirements add Linux's inode model, orphans, unwritten and NOCOW extents, and stable directory cookies | ROADMAP §14.8, §14.9, §17.1, §23.1, §29.5, §31.8; DESIGN §2.7; VIBEFS §2, §11, §15 | #92 |
| J092 | Major | Writeback reports errors as Linux's errseq does, and data checksums cover exactly the bytes written | ROADMAP §12.5, §19.8, §23.1; DESIGN §10.6; VIBEFS §10, §15 | #92 |
| J093 | Major | full-disk encryption is LUKS2, loaded through one dm-crypt key path from §18.7 on | ROADMAP §18.7, §29.1, §29.3, §36.1, §37.2; DESIGN §2.10 | #92 |
| J094 | Major | A/B installs keep user state on a shared partition, and no release sets a feature bit the other slot cannot read | ROADMAP Phase 22, §22.1, §22.2, Phase 26, §26.2, §37.2, Phase 39, §39.2; VIBEFS §14, §15 | #92 |
| J095 | Major | Keep the formats releases share from Phase 22's release on | ROADMAP §14.6, §20.1, §22.1, §22.2, §25.4, §30.2, Phase 39, §39.1, §39.2; DESIGN §2.5; VIBEFS §14, §15 | #92 |
| J096 | Major | Schedule md check, not repair, and give stacked targets their copies | ROADMAP Phase 29, §29.1, §29.4 | #92 |
| J097 | Major | An encrypted trial boot waits for its passphrase, and a trial that never reached its root is retried | ROADMAP Phase 22, §22.2, §25.5, Phase 37, §37.2 | #92 |
| J098 | Minor | Run the vibefs crash test over the volatile-cache device from Phase 10 | ROADMAP §8.5, Phase 10, §10.2, §10.11, §12.5, §14.8, Phase 22, §23.2, Phase 25, §25.7, Phase 29, §29.2, Phase 30; DESIGN §8.3, §10.6; VIBEFS §12 | #92 |
| J099 | Major | Block error handling has one claim per request, one handler per unit, and one quiesce | ROADMAP §10.11, §11.3, §12.5, §18.1, §20.2, §20.4, §20.5, §20.9, §25.4, §29.1; DESIGN §2.2, §4.4, §5.4, §10.1, §10.3, §10.4, §12.1, §12.2, §12.4; VIBEFS §10 | #92 |
| J100 | Major | Every hang detector sits above the storage stall bound, and a wedged RAID member is gated | ROADMAP §12.4, §12.5, §22.2, §23.3, Phase 25, §25.5, Phase 29; DESIGN §8.3, §10.3 | #92 |
| J101 | Major | One lockless, sequence-numbered log ring with a printer per console; markers stay synchronous | ROADMAP §5.5, §13.9, §19.5; DESIGN §2.3, §2.5, §2.6, §7.7 | #92 |
| J102 | Major | Define 'nothing is silently swallowed' and enforce it with clippy's discard lints | ROADMAP §10.1, §10.11; DESIGN §2.5, §2.7; AGENTS | #92 |
| J103 | Major | Bound the persistent panic record and the retired-frame list, and delete records once logged | ROADMAP §20.1, Phase 25, §25.3, §25.5, §25.6; DESIGN §2.5 | #92 |
| J104 | Major | The panic order branches, so a capture jump calls no firmware and does not pause the VM | ROADMAP §20.1, §22.2, Phase 25, §25.4, §25.5, §25.6; DESIGN §2.5, §5.2 | #92 |
| J105 | Minor | The harness follows QMP panic events, and cores are physical and read through one VMCOREINFO note | ROADMAP §10.2, §10.7, §11.7, §18.2, §22.2, §25.4, §27.3; DESIGN §3.3, §8.3, §8.4 | #92 |
| J107 | Minor | Move the entropy pool and CSPRNG to §13.10 so AT_RANDOM never fails or waits | ROADMAP §10.12, Phase 13, §13.10, §14.7, §15.1, §15.11, §18.2, §18.7, §25.4; DESIGN §1.5; SYSCALL §7; issue plans S1 | #92 |
| J108 | Major | Signed update metadata refuses rollback and expires, and the root set rotates | ROADMAP §10.1, §14.6, §22.1, §22.2, §39.1, §39.2 | #92 |
| J109 | Major | Crypto comes from pinned RustCrypto and dalek crates behind one facade, and TLS from rustls | ROADMAP §14.7, §15.11, §18.7; DESIGN §1.5 | #92 |
| J110 | Major | Test trust anchors reach only harness images, and the release workflow refuses one | ROADMAP §14.3, §14.6, §15.11, §18.5, §21.6, Phase 24, §24.2, §31.6, §36.4, Phase 37; AGENTS | #92 |
| J111 | Major | Frame kernel serial lines with 0x1E so user output cannot forge a marker or fail a run | ROADMAP §10.2, §13.7, §13.13; DESIGN §2.6, §2.7, §8.2, §8.3, §9.7 | #92 |
| J113 | Major | Strict IOTLB by default, each CPU vulnerability mitigated or owner-accepted, and canaries and uapi padding specified | ROADMAP §10.6, §13.13, Phase 18, §18.1, §18.3, §18.4, §21.1; DESIGN §2.4; AGENTS | #92 |
| J114 | Major | Build releases in one pinned image, compare before each of two key jobs, and publish the root fingerprints | ROADMAP §14.6, §18.7, Phase 22, §22.1, §22.3, §22.4, §24.1, §24.2, §25.4, Phase 39, §39.3, Public clouds; DESIGN §8.6 | #92 |
| J115 | Major | Ship a port only when two builds of the release commit's key agree, and check artifacts that cross runs | ROADMAP Phase 24, §24.2, §39.3; AGENTS | #92 |
| J116 | Major | Check gate changes against the merge base, take review corrections as errata, and require CI on main | ROADMAP standing gates, Phase 10, §10.9; DESIGN §8.6; AGENTS; PR template; `KERNEL_REVIEW.md` | #92 |
| J117 | Major | Take a kexec trial boot only when the slot's whole firmware path is unchanged on this machine | ROADMAP Phase 30, §30.4 | #92 |
| J118 | Major | Define the trial-boot health check locally, with a baseline and a deadline, and never retry a failed release | ROADMAP Phase 22, §22.2 | #92 |
| J119 | Major | Each release installs its own boot chain, and updates are tested from the major's first release | ROADMAP §14.6, §22.1, §22.2, Phase 39, §39.1 | #92 |
| J120 | Major | Bootstrap the handover format at phase-25 and refuse an unreadable handover at load time | ROADMAP §25.4, §30.4, §30.6 | #92 |
| J121 | Major | /dev/watchdog lands in §22.2, armed early by the kernel | ROADMAP §20.6, Phase 22, §22.2, §22.3, Phase 25, §25.5 | #92 |
| J122 | Minor | Host each architecture's repository and the update channel on this repository's releases over HTTPS | ROADMAP §14.6, §15.8, Phase 22, §22.1, §22.2 | #92 |
| J123 | Major | Thin pools wait in §29.6 for a metadata write-up from pools Linux wrote | ROADMAP §29.1, §29.3, §29.6; DESIGN §1.5; AGENTS | #92 |
| J124 | Minor | Page tables stay in vibeos-core, and the Verus spike tries one | ROADMAP §12.1, §38.2; DESIGN §11.1 | #92 |
| J125 | Minor | Phase 38 proves frame-refcount steps in Verus and model-checks their protocol in TLA+ | ROADMAP §12.1, Phase 38, §38.1, §38.2, §38.3, §38.4 | #92 |
| J126 | Major | The page-table proof models hardware-set dirty bits, DBM, block leaves, and every root | ROADMAP §12.1, §12.2, Phase 38, §38.2; DESIGN §4.3 | #92 |
| J127 | Major | List the proof's whole trusted base and what it does not prove | ROADMAP §12.1, Phase 38, §38.1, §38.2, Beyond | #92 |
| J128 | Minor | Say what a change to proved code owes after phase-38, and scope Phase 38 to what Phase 27 has landed | ROADMAP Phase 27, §27.3, Phase 38, §38.1; DESIGN §8.6 | #92 |
| J129 | Major | Sanitizer hooks for every allocator, fuzz coverage per interface | ROADMAP §12.1, Phase 18, §18.3, §18.4, §18.5, §19.3, §19.5, §19.8, §19.9; DESIGN §4.4 | #92 |
| J130 | Major | S3 resume, the RTC alarm wake, and the clock rebase land in §20.2 | ROADMAP Era IV. Frontier, Phase 20, §20.2, §31.3; DESIGN §6.6, §7.4 | #92 |
| J131 | Minor | Record nesting from Phase 11 and measure the Phase 24 toolchain builds in Phase 17 | ROADMAP How to read this, §11.7, §17.7, Era IV. Frontier, §20.8, Phase 21, §21.1, Phase 22, Phase 24, §24.1, Era VI. Production | #92 |
| J132 | Minor | Run Phase 37's workload on the release image and its Linux ratio on Alpine's binaries | ROADMAP Phase 24, Era VII. Daily Driver, §36.4, Phase 37, §37.1, §37.2, §37.3 | #92 |
| J133 | Major | Soundness boxes name their proofs, and check_cells.py checks bounds, unsafe fns and provenance | ROADMAP §10.3, §12.5; DESIGN §2.3, §2.7, §5.1; AGENTS; issue plans Q3 | #92 |
| J134 | Major | Clippy enforces rule 4 in the byte parsers and across the kernel binary | ROADMAP §10.1; DESIGN §2.5; AGENTS; `clippy.toml`, `src/lib.rs`, `tests/gates/phase-10-needs.toml`; issue plans E1, index | #92 |
| J135 | Major | A race fix is proved by a test that fails before the fix, and a retried run is not green | ROADMAP How to read this, standing gates, §10.2, §10.3, §10.6, §10.8, §10.9, §10.10; DESIGN §5.4, §8.2, §9.4, §9.8; PR template | #92 |
| J136 | Major | A ticked box whose proof cannot fail is open; deferral boxes close with their landing box; this register | ROADMAP How to read this, standing gates, Phase 0, Phase 1, Phase 2, Phase 3, Phase 4, §4.3, §4.11, §5.5, §6.4, Phase 7, §8.4, §8.5, §8.6, §9.4, §9.5, §10.1, §10.2, §10.5, §10.9, §10.10, §14.3, §14.4, §14.6, §14.10, §37.4; DESIGN header, §2.10; README; CHANGELOG; `tests/gates/phase-10-needs.toml`, `tests/gates/phase-11-needs.toml` | #92 |
| J137 | Major | check_ticks pairs each Proves trailer with its box and requires a proof that ran | ROADMAP How to read this, §10.9; DESIGN §8.4, §8.6; AGENTS; PR template; `tests/gates/phase-10-needs.toml` | #92 |
| J138 | Major | The in-guest verdict counts runs, pins expected skips, and times each test, not the boot | ROADMAP §10.1, §10.2, §10.5, Phase 11, §11.6, §11.7, Phase 17, §17.6, §21.7; DESIGN §8.2, §8.3, §8.4, §8.6; AGENTS; `tests/gates/phase-10-needs.toml`; issue plans C2, Q1 | #92 |
| J139 | Major | Phase 10's code-shape gate lines name the scripts that keep them true | ROADMAP How to read this, standing gates, Phase 10, §10.1, §10.2, §10.3, §10.4, §10.6, §10.9; DESIGN §11.1; `tests/gates/phase-10-needs.toml` | #92 |
| J140 | Major | A phase closes only when its section boxes are ticked or deferred | ROADMAP How to read this, standing gates, §9.3, §9.7, §10.9, §22.3; AGENTS | #92 |
| J141 | Minor | The issue index's Status column is the one status of a letter code | ROADMAP Phase 10, §10.1, §10.2, §10.5, §10.8; issue plans A1, A2, B1, B2, B3, B4, C2, D3, DOC1, DOC3, DOC4, DX1, E1, E3, O1, P2, Q3, Q4, R1, index, S1, T2, T3 | #92 |
| J142 | Major | One precedence rule, and each open plan names what its boxes supersede | ROADMAP How to read this, Phase 10, §10.2, §10.3; DESIGN header; AGENTS; issue plans A1, A2, A3, A4, B3, B4, D1, D2, DOC2, DOC4, DX1, E1, E2, I1, P1, Q1, Q2, Q5, R1, index, S1, T1, T4 | #92 |
| J143 | Major | One marker registry replaces four copies of the boot contract | ROADMAP How to read this, standing gates, §10.2, §11.1, §11.2, §11.7; DESIGN §8.3 | #92 |
| J144 | Major | Memory gates count what they claim: crash epochs, zero-page faults, exhaustive accounting, heap bytes | ROADMAP Phase 12, §12.1, §12.2, §12.5, §12.6, §13.13, Phase 25, §25.7, Phase 27, Phase 30, Phase 39, Long runs and scale; DESIGN §4.4 | #92 |
| J145 | Minor | aarch64 guests run on arm64 runners, and HVF records loop the -smp 4 tier | ROADMAP standing gates, §10.1, §10.8, Phase 11, §11.7; DESIGN §8.6 | #92 |
| J146 | Major | Enable only the hypervisor paths a gated run covers, cover each in the kvm-unit-tests line, and fund one Intel and one AMD test PC | ROADMAP How to read this, Phase 21, §21.1, Phase 22, Era VI. Production, Bare metal and hardware CI | #92 |
| J147 | Major | Speed thresholds under KVM are stated against Linux in the same job | ROADMAP How to read this, Phase 12, Phase 15, Phase 16, §16.1, Phase 19, §19.3, §19.4, Phase 25; DESIGN §8.6 | #92 |
| J148 | Major | A ticked box keeps its text, and funded goals add lines, lapse, and disclose moves | ROADMAP How to read this, §10.9, §17.7, Phase 21, §21.1, §22.1, §22.3, §39.3, Funded goals, Bare metal and hardware CI, Hosted CI capacity, Long runs and scale; AGENTS; PR template | #92 |
| J149 | Major | Scheduled CI runs in FIFO lanes, and a release candidate heads a release branch | ROADMAP How to read this, standing gates, §10.1, §10.2, §10.9, §14.9, §17.7, §22.1, §24.2, §29.5, Phase 39, §39.2, §39.3, Hosted CI capacity; DESIGN §8.6 | #92 |
| J150 | Major | Fuzz crashes stay sealed until fixed, and the embargo gets a release path | ROADMAP §10.1, §10.7, §14.10, Phase 22, §22.5, Phase 39, §39.3; DESIGN §1.5, §8.6 | #92 |
| J151 | Minor | The file is the license unit, adapted code is notice-only, and patented formats wait for the owner | ROADMAP §10.9, §11.6, §12.2, §14.10, §29.2, Phase 36, §36.6, §37.1, §37.2, Daily-driver hardware; DESIGN §1.5; AGENTS | #92 |
| J152 | Minor | The physmap gets a slot, every kernel PDPT exists at install, and memremap maps what the kernel does not own | ROADMAP §11.1, §11.2, Phase 12, §12.1, Phase 18, §18.1, §18.2, §20.1, §25.4, §27.6, Phase 38, §38.2; DESIGN §2.7, §4.1, §4.3, §11.2 | #92 |
| J153 | Minor | Lower layers call up only through listed init hooks, and one allocation entry reaches reclaim | ROADMAP §10.3, §12.6; DESIGN §1.1, §1.2, §4.4 | #92 |
| J154 | Minor | The AP trampoline page comes from the memory map, and the buddy's exclusions are stated once | ROADMAP §10.6, §20.2; DESIGN §2.4, §2.7, §4.1, §4.2, §7.3, §9.2, §11.1 | #92 |
| J155 | Minor | x86 AP bring-up failure lands in §11.4 beside aarch64's, and gate 508 says what it proves | ROADMAP Phase 4, §4.5, §11.4, §20.1, §27.5; DESIGN §2.8, §7.4, §7.11, §9.5 | #92 |
| J156 | Minor | An unsupported operation returns Linux's errno for it, and FAT zeroes the slack an extension exposes | ROADMAP §10.4, §12.5, §13.9, §26.4; VIBEFS §1; issue plans A3 | #92 |
| J157 | Minor | `vibeos-core` gets an MSRV that `make check` builds, and Phase 10's Cargo mechanisms work | ROADMAP §10.1, §10.2, §10.4, §12.1, §38.1; DESIGN §1.1, §3.1, §4.4 | #92 |
| J158 | Minor | The kernel is built and linted with warnings denied from wave 1, and releases ship the release profile | ROADMAP How to read this, Phase 10, §10.1, §10.2, §10.8, §10.9, §18.4; DESIGN §2.5, §3.5; SYSCALL §3.1; VIBEFS §3; AGENTS; `Cargo.toml`, `tests/gates/phase-10-needs.toml`; issue plans DX1 | #92 |
| J160 | Minor | Record that disk encryption gives secrecy, not integrity, and ask the owner before it is on by default | ROADMAP §18.8, Beyond | #92 |
| J161 | Minor | Paid goals state their recurring costs, the $0 Khronos route and buy lists that work on arrival | ROADMAP Funded goals, Public clouds, Hosted CI capacity, Daily-driver hardware, Paid services | #92 |
| J162 | Minor | Publish-last is Release and Acquire, its hand-offs get models, and the clock latch is safe in NMI | ROADMAP §10.3, §10.4, §10.8, §10.10, §11.3, §11.4, §11.7; DESIGN §2.5, §2.8, §6.4, §7.5, §7.6, §9.4, §10.1; AGENTS; issue plans A4, D1 | #92 |
| J163 | Minor | Move threads between CPUs only through inboxes, and plan the SCHED split | ROADMAP §10.7, §10.10, §13.12, §19.4; DESIGN §7.7, §9.4 | #92 |
| J164 | Minor | Faulting and non-faulting user-memory accessors, each with its fault contract | ROADMAP §10.6, §12.2, §12.5, §13.12; DESIGN §2.1, §2.2, §2.5, §2.7, §2.9, §5.1, §5.2, §11.5 | #92 |
| J165 | Minor | Reserve the frame-metadata and KASAN-shadow regions, and map the shadow from _start | ROADMAP §12.1, §18.2, §20.1, §25.4, §27.3; DESIGN §3.3, §4.1, §11.2 | #92 |
| J166 | Minor | User address-space limits follow Linux from Phase 12 | ROADMAP Phase 10, §10.4, §10.6, §12.2, §12.4, §23.4; issue plans D1 | #92 |
| J167 | Minor | The switch tail caches dead stacks and never unmaps | ROADMAP §10.10, §12.1, Phase 25; DESIGN §4.5 | #92 |
| J168 | Minor | The hypervisor and pseudo-NMI boxes name the CPU state and entry paths they change | ROADMAP §21.1, §25.5 | #92 |
| J169 | Minor | §19.9's slab drops constructors, hands out kalloc types, and gates on the lock it relieves | ROADMAP Phase 19, §19.9, §19.10; DESIGN §4.4, §4.6 | #92 |
| J170 | Minor | Reclaim skips mlocked pages, and §19.4 names Linux's scheduling interface for rtkit | ROADMAP §12.4, §19.4, §34.4; DESIGN §4.4 | #92 |
| J171 | Minor | A page two PT_LOADs share gets the later segment's permissions, as on Linux, and user binaries never share one | ROADMAP §10.5, §10.6, §12.2; SYSCALL §7 | #92 |
| J172 | Minor | User-visible behaviour follows Linux on each architecture, and aarch64 keeps TBI0 with Linux's untagged syscall ABI | ROADMAP How to read this, Phase 11, §11.1, §11.6, §18.4, Phase 23; DESIGN §11, §11.2, §11.4; SYSCALL header | #92 |
| J173 | Minor | §15.6 names Linux's TCP defaults and off-path defenses, and caps packetdrill's list | ROADMAP Phase 15, §15.4, §15.6, §15.8, §15.10; DESIGN §2.10 | #92 |
| J174 | Minor | Kexec parks x86 APs in INIT, and a capture kernel stops inherited DMA first | ROADMAP §18.1, §25.4; DESIGN §2.8, §4.7 | #92 |
| J175 | Minor | Machine-check recovery runs in exit work, the SRAR gate injects at CPL 3, and the capture kernel feeds the watchdog | ROADMAP §20.6, Phase 25, §25.1, §25.3, §25.4; DESIGN §2.5, §5.2, §5.10 | #92 |
| J176 | Minor | EFI runtime calls run on one efi_rt thread with interrupts on and a 1:1 map, and a firmware record marks CPUs in firmware | ROADMAP §20.9, §25.4, §25.5, §25.6, Phase 26; DESIGN Contents, §2.5, §2.9, §4.8 | #92 |
| J177 | Minor | The blocked-thread sweep reads recorded deadlines, and the lockup detectors take Linux's thresholds | ROADMAP §10.7, §13.5, Phase 25, §25.5, §27.5, Bare metal and hardware CI; DESIGN §6.5, §8.3 | #92 |
| J178 | Minor | Pid 1's death is the one way ring 3 ends the system, and its panic names the cause | ROADMAP §10.5, Phase 20; DESIGN §2.5, §2.7, §2.10, §10.3; AGENTS | #92 |
| J179 | Minor | One Limine base revision, stated boot options, and whole EL2 state on aarch64 secondaries | ROADMAP §11.1, §11.4, §18.2; DESIGN §2.9, §3.2, §11.1; `limine.conf` | #92 |
| J180 | Minor | Every ECAM window is stored by its first bus, and MCFG bases are moved there when parsed | ROADMAP §11.5, §20.1; DESIGN §9.2 | #92 |
| J181 | Minor | Size MAX_META to v1's worst case, tie the crash criterion to fsync, and drop the mirror | ROADMAP §10.11, §38.3; DESIGN §8.3; VIBEFS §6, §10, §12 | #92 |
| J182 | Minor | Leap seconds follow Linux's STA_INS and STA_DEL stepping, never a kernel smear | ROADMAP §30.5 | #92 |
| J183 | Minor | DMA limits, the direct entry's layout logic, and aarch64 sigreturn code each have one home | ROADMAP §13.8, §13.10, §25.4, §27.2; DESIGN §4.1, §11.1 | #92 |
| J184 | Minor | Rules mark what is not built yet, rule 4 lists the untrusted sources once, and README stops overstating safety | DESIGN §2.10, §4.4, §8.1; AGENTS; README; `tests/gates/phase-10-needs.toml`; issue plans S1 | #92 |
| J185 | Minor | Every stated rule gets a register row, and SAFETY comments cite it | ROADMAP standing gates, §10.1, §10.3, §18.4; DESIGN §2.7; AGENTS | #92 |
| J186 | Minor | Fix boxes name proofs that fail on the unfixed code | ROADMAP How to read this, §9.4, §10.1, §10.2, §10.3, §10.6, §10.10, §10.11, §11.6, §13.1, §18.3, §19.6, §20.1; DESIGN §2.5, §9.1 | #92 |
| J187 | Minor | A checkbox is work with a proof; intent and decisions become prose | ROADMAP How to read this, §10.9, Phase 11, §12.1, Era III. Platform, Phase 14, §14.1, §14.3, §15.6, §15.9, §16.3, §16.7, §19.9, Phase 20, §37.4 | #92 |
| J188 | Minor | A pull-request run proves no commit, and no main run is cancelled | ROADMAP How to read this, §10.1, §10.9, Bare metal and hardware CI; DESIGN §8.6 | #92 |
| J189 | Minor | `make gate` covers Phases 8 and 9; check where findings' boxes sit | ROADMAP Phase 10, §10.9; CHANGELOG; `KERNEL_REVIEW.md` | #92 |
| J190 | Minor | Fix the QEMU setup: aarch64 argv, edu mask, firmware pairs, cores | ROADMAP §10.2, §10.7, Phase 11, §11.5, §11.7, §18.1, §25.7; DESIGN §8.4; CHANGELOG; issue plans A2, I1 | #92 |
| J191 | Minor | A nightly mm_compose test runs Phase 12 and 13's memory paths together under lockdep and KASAN | ROADMAP Phase 13, §13.12, §13.13 | #92 |
| J192 | Minor | Five later gate lines fail on the bug and run as written | ROADMAP How to read this, §13.9, §18.5, Phase 21, §21.8, Phase 22, §22.2, Phase 25, §25.2, Bare metal and hardware CI | #92 |
| J193 | Minor | Name each specification's trace events and order traces by causality | ROADMAP §10.10, §38.4, §38.5; DESIGN §7.9 | #92 |
| J194 | Minor | Decide `/dev/log`'s owner, who makes by-id links, window decorations, and the host mount | ROADMAP §16.5, §16.6, §17.4, §20.9, Phase 23, §23.5, Phase 30, §30.3, Beyond | #92 |
| J195 | Minor | Define the terms later gates depend on | ROADMAP How to read this, §20.1, §22.1, §22.4, Phase 24, §24.5, Phase 25, §25.5, §25.6, Phase 27, §27.4, Phase 29, §29.4, Phase 33, §33.3, §37.4, Funded goals | #92 |
| J196 | Minor | Run Phase 21 beside Phase 20 and let Phase 22 need only its container sections | ROADMAP The arc, Era IV. Frontier, Phase 21, Phase 22, §22.1, §22.4, Era VI. Production, §25.4 | #92 |
| J197 | Minor | Gate the aarch64 direct entry's VMM boot under QEMU -kernel, and make the Oracle Cloud backend a stretch | ROADMAP Phase 26, §26.4, §26.6, §26.7, Public clouds | #92 |
| J198 | Minor | Funded hardware lines read metered outlets and name their pass criteria | ROADMAP Funded goals, Bare metal and hardware CI, Public clouds | #92 |
| J199 | Minor | Native builds say why and check against acpiexec, busybox, crun and cloud-init | ROADMAP §14.4, §14.5, Phase 20, §20.2, §20.8, Phase 21, §21.6, §26.2, Beyond | #92 |
| J200 | Editorial | Match DESIGN to its own constraints, marker grammar and single-home constants, and fix wrong citations | ROADMAP The arc, Phase 10, §10.3, Phase 12, §12.7, §14.3, §17.7, §19.10, §23.2, Era VIII. Assurance; DESIGN §1.1, §1.4, §2.6, §2.7, §3.1, §3.3, §3.6, §5.4, §6.1, §8.6; SYSCALL §1; `.github/workflows/ci.yml` | #92 |
| J201 | Editorial | Review documents name their PRs and the issue index's tiers stop reading as phases | `ARCHITECTURE_REVIEW.md`; issue plans A4, index | #92 |
| J202 | Editorial | Correct stale scaffolding comments, gate FAIL_NEXT with the test hooks, and drop wrong cites | ROADMAP §10.2, Phase 13, Public clouds, Hosted CI capacity; CONTRIBUTING; CHANGELOG; `.gitignore`, `Makefile`, `src/arch/gdt.rs`, `src/arch/trampoline.S`, `src/fb_init.rs`, `src/log_init.rs`, `src/main.rs`, `src/paging.rs`, `src/paging_init.rs`, `src/per_cpu.rs` | #92 |
