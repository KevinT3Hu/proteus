# Production readiness review

Reviewed 2026-10-24 against the current working tree, including existing uncommitted changes. This is a source and local-test review, not a penetration test or a compositor/GPU certification run.

## Hardening follow-up (2026-10-24)

The findings below preserve the original review. The following changes now address findings 1–6 and 8, plus the native cache, graphics-file, decoder-queue, and clipboard hardening items:

- Shell integration requires private user-owned directories, rejects symlinked/untrusted ancestors, and replaces scripts using exclusive private staging files and directory-relative rename.
- PTY output retains byte/message permits through GUI consumption: 64 KiB per pane and 512 KiB per process, in 8 KiB chunks. Full queues backpressure readers; closing cancels waits. Consumption is deferred to idle processing so producers cannot refill the event source continuously before input/render work runs.
- Plugin host calls are acknowledged after GUI consumption, have a 4 MiB message limit and a 2000 calls/second limit, and retain separate image quotas. Incoming plugin events are bounded at 256; overflow stops the instance. Pending questions/actions and desktop opener workers are bounded.
- Custom plugin filesystem helpers retain grant-root descriptors and resolve targets/parents with `openat2` containment. Rename endpoints are pinned separately; trash destinations are private and no-replace publication protects existing trash files. Linux 5.6+ and `/proc` are required. Native process execution remains intentionally outside the WASM sandbox when granted.
- Hyperlinks no longer accumulate forever: unused slots are reclaimed against live cell styles and active/saved cursor state.
- Layout Lua has a 1 MiB source limit, 16 MiB allocation limit, 100 ms instruction-hook deadline, and structural depth/size/cycle validation.
- Disk-native WASM cache deserialization was removed; compiled components are cached only in memory (up to 16).
- Graphics file reads pin regular files before opening for I/O, reject symlinks during resolution, and use nonblocking reads. Decoder admission limits queued/running/completed jobs together to 32 and 512 MiB of reserved encoded/output/scratch space.
- Clipboard receives are limited to four concurrent transfers, 16 MiB each, and three seconds. Timeout/overflow discards the entire transfer; drag/drop receive pipes also have a deadline.
- `.github/workflows/ci.yml` installs the WASI target, requires real WASM execution, and runs strict lint/format checks, workspace tests, standalone SDK/file-manager tests, component builds, and a release build. Missing targets fail tests whenever `CI` or `PROTEUS_REQUIRE_WASM_TESTS` is set. Explicit prebuilt fixture input is supported for local toolchains without packaged WASI libraries.

Regression coverage includes hostile shell integration directories/symlinks, directory/grant-root swaps, queue cancellation and overflow, plugin flood backpressure with a real component, hyperlink churn and saved cursors, decode admission, stalled/oversized clipboard pipes, and runaway/cyclic/oversized layouts. Local validation passed 252 workspace tests including seven actual WASM integration tests (one unrelated throughput test remains ignored), 16 SDK unit tests plus two doctests, and 30 file-manager tests. Strict workspace Clippy and formatting passed; the file manager also built as a release WASI component. Local WASI validation used a temporary standard-library/sysroot setup and a freshly built fixture. The GitHub workflow itself has not yet run on a hosted runner.

Finding 7 is now addressed by the persistence follow-up below. Packaging, live desktop compatibility/latency evidence, and the remaining product/release items are still open. These changes are not a claim that the whole project is production-ready.

## Persistence follow-up (2026-10-24)

Finding 7 is addressed with a shared descriptor-relative state-file helper: private files/directories, exclusive staging, file synchronization, atomic rename, and directory synchronization. Sessions, saved layouts, palette history, and plugin storage use it. Explicit layout directories retain their existing permissions.

Plugin storage reloads under a stable interprocess lock for every operation, avoiding speculative in-memory commits and lost updates between instances. Read, parse, write, and delete failures are surfaced; malformed storage is preserved. Corrupt session snapshots receive a private durable recovery backup before replacement; unreadable/oversized snapshots block autosaves. Failed saves retry, including when rename succeeded but synchronization failed. Such an error can leave the new snapshot visible; durability depends on filesystem synchronization support. Restoring a layout/cwd does not restore running jobs.

Regression coverage includes injected disk-full and permission failures, interruption before rename, restart/recovery, hostile links, simultaneous subprocess writers, malformed/oversized state, and error reporting through an actual WASM component. Final local validation passed 265 workspace tests including eight real WASM integration tests (one throughput test remains ignored), formatting, strict Clippy, and a workspace build.

## Assessment

Proteus has substantial implementation and useful regression coverage, but should currently be treated as an engineering alpha. The main production gaps are host resource isolation, filesystem trust boundaries, enforced validation, distribution, and real desktop compatibility evidence. Adding more features should follow those foundations.

Linux/Wayland-only support and the absence of detachable sessions are explicit product decisions, not deficiencies. AI is a separate release-scope decision: the design promises it, but this checkout does not contain the described AI/provider plugins.

## Validation performed

- `cargo test --workspace --locked`: successful; 243 tests reported passed, one ignored. Six of those reported passes are WASM integration tests that return early without exercising a component on this machine.
- `cargo test -p proteus-wasm --test runtime --locked -- --nocapture`: confirmed `skipping: no wasm32-wasip2 target`. This makes the distinction above material.
- `cargo test --manifest-path plugins/files/Cargo.toml --locked`: 30 passed.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: failed at `crates/proteus-core/src/graphics/kitty.rs:127` (`chunks_exact_to_as_chunks`). Also reported an unused `base64` dependency in `proteus-wasm`.
- No live GUI, compositor, physical GPU, screen-reader, release-package, dependency-advisory, or long-running soak validation was performed. Test success does not establish those properties.

## Findings, in priority order

### 1. P1: Shell-integration fallback directory is not a secure execution boundary

Evidence: `crates/proteus-pty/src/shell.rs:41–79`.

Without `XDG_RUNTIME_DIR`, integration goes under the predictable temporary path `proteus-UID`. Installation uses ordinary directory creation and path-based reads/writes; it does not establish that existing directories belong to the current user, are private, or are not symlinks. The installed files are subsequently sourced by the user's shell. On a shared machine, a pre-created attacker-controlled tree can therefore become a shell-code injection boundary. Normal private XDG runtime directories reduce exposure but do not make the fallback safe.

Required: securely acquire a user-owned 0700 runtime directory, reject hostile existing objects, and perform safe file creation/replacement relative to trusted directory descriptors. Add tests for a pre-created directory, symlinked parent/file, wrong owner, and concurrent starts. Fail closed or disable integration when its directory cannot be trusted.

### 2. P1: PTY output can consume unbounded host memory

Evidence: `crates/proteus/src/gui/pane.rs:65–78`, `crates/proteus/src/gui/mod.rs:658–668`.

Reader threads repeatedly allocate up to 64 KiB messages and send them to the shared unbounded GUI channel. A noisy process or several active panes can outpace terminal parsing/rendering. Scrollback and image limits do not bound queued output. Memory exhaustion can terminate every pane in the process.

Required: per-pane and process-wide byte limits with cancellable backpressure, fair scheduling, and a bounded amount of parsing per event-loop iteration. Preserve byte order and terminal stream correctness. Validate a flood in one pane while typing into another, with a defined memory ceiling and input-latency target.

### 3. P1: Plugin one-way calls bypass host queue limits

Evidence: `crates/proteus-wasm/src/runtime.rs:142–172,423–428`, `crates/proteus/src/gui/plugins.rs:417–421`.

One-way drawing, logging, status, prompt, and related calls enqueue owned host messages without waiting for consumption. A guest's 256 MiB memory limit and five-second call deadline do not cap allocations already transferred into the host queue. The existing process/HTTP backlog accounting covers a different direction of traffic.

Required: budget plugin-to-host messages by bytes and count, coalesce superseded frames/status, rate-limit notifications/logs/prompts, and stop abusive instances without blocking the GUI. Test repeated large frames and log floods from a real component.

### 4. P1: Custom plugin filesystem operations have check/use races

Evidence: `crates/proteus-wasm/src/files.rs:36–77`, `crates/proteus-wasm/src/runtime.rs:839–917`.

`Access::resolve` canonicalizes and authorizes a pathname, then returns a `PathBuf`. Operations subsequently open, rename, chmod, list, or trash through that pathname. A writable intermediate directory can be renamed and replaced with a symlink between authorization and use, redirecting the operation outside the grant. `O_NOFOLLOW` on the final chmod target does not secure intermediate components. This is a source-confirmed race window; an exploit was not executed during this review.

Required: retain capability directory descriptors and use descriptor-relative operations with containment enforcement (`openat2` resolution restrictions where suitable). Apply the same approach to both rename endpoints and other custom host calls. Add adversarial directory-swap tests. Do not assume WASI's own path isolation protects these separate host helpers.

### 5. P1: WASM runtime integration tests can silently provide false confidence

Evidence: `crates/proteus-wasm/tests/runtime.rs:13–45`; reproduced locally.

The six integration tests are reported as successful when their component target is missing. This leaves actual instantiation, sandbox enforcement, timeouts, and SDK/host interoperability untested in a normal green workspace run.

Required: a mandatory job with the target installed and `PROTEUS_REQUIRE_WASM_TESTS=1`. Build and test the SDK and file manager separately as well: their standalone workspaces are outside `cargo test --workspace`. Build the file manager for `wasm32-wasip2` and exercise its workflows through the real host.

### 6. P2: Hyperlink metadata grows for the lifetime of a terminal

Evidence: `crates/proteus-core/src/style.rs:113–138`, `crates/proteus-core/src/term.rs:1676–1684`.

Every distinct OSC 8 hyperlink is retained in a vector and hash map, with no reclamation when associated cells leave scrollback. URI length limits bound individual entries, not the table. Continuous unique hyperlinks can exhaust memory despite bounded history. Style compaction already exists, but does not reclaim this separate table.

Required: reclaim links against live cells and cursor/saved state, or apply a safe bounded-table policy. Test millions of distinct links with a small scrollback and verify memory stabilizes.

### 7. P2: Persistence is atomic at rename, but not durable or fully failure-consistent

Evidence: `crates/proteus/src/gui/workspace.rs:41–69`, `crates/proteus-wasm/src/storage.rs:25–64`.

Writes use temporary files plus rename, which protects against partial readers, but omit file and directory synchronization and explicit private modes. Session snapshots may contain commands and working directories. Plugin storage mutates its in-memory map before saving; a failed save leaves memory ahead of disk, and repeating the same set can return success without retrying persistence. Delete failures are discarded.

Required: a shared private, durable atomic-write helper; commit in-memory storage changes only after persistence succeeds or track dirty state and retry. Propagate meaningful failures. Test disk-full/permission failures, restart after interrupted writes, simultaneous processes, and recovery from malformed state. State clearly that restored layout/cwd does not restore running jobs.

### 8. P2: Layout evaluation can hang the GUI or startup

Evidence: `crates/proteus/src/spec.rs:371–378` and layout/session loading in `gui/workspace.rs`.

Layout files are evaluated as Lua with no execution hook or memory budget in this parser. A mistaken infinite loop or allocation loop can prevent startup/session restore or freeze a layout action. `ALL_SAFE` concerns Lua library safety; it is not a CPU or memory limit.

Required: execution and allocation limits, structural depth/node-count bounds, and a recovery path that starts without a broken layout/session. Test infinite loops, cyclic/deep tables, and oversized layout files.

### 9. P1 release gate: No checked-in release/CI/distribution foundation

Evidence: repository inventory and root `Cargo.toml`.

No CI workflows, pinned toolchain file, package definitions, desktop entry, application icon/metainfo, release procedure, or license texts were found. The manifest declares dual licensing and Rust 1.97 but does not supply the corresponding reproducible installation/release workflow. Source dependencies are pinned and lockfiles are present, which is a useful start.

Required before a public stable release:

- Pin and document the supported toolchain and native build dependencies; build on a clean supported distribution and establish the minimum libc/runtime baseline.
- CI for formatting, strict Clippy, workspace tests, standalone SDK/file-manager tests, mandatory WASM tests, and release builds. Fix current lint failures.
- Dependency vulnerability/license checks across all lockfiles, license texts, third-party notices, and an artifact inventory/SBOM as appropriate to the distribution channel.
- Installable packages with desktop integration, bundled/precompiled first-party WASM, terminfo and shell integration, verified checksums/signatures, installation/uninstallation instructions, and rollback/version compatibility policy.
- A root README covering support scope, build/run, defaults/keybindings, configuration, troubleshooting, and known limitations.

### 10. P1 release gate: Desktop correctness and performance lack demonstrated acceptance evidence

Evidence: acceptance matrix in `docs/design/00-overview.md`; current tests are primarily unit/headless integration tests. `crates/proteus-core/tests/replay.rs:91` is an ignored parser throughput measurement, not an end-to-end latency or resource test.

The design correctly calls for KWin/Sway/Mutter, fcitx5, Intel/AMD/NVIDIA, and Vulkan/GL validation. This review found no checked-in results or automated desktop acceptance harness establishing those criteria. Existing IME, accessibility, text shaping, graphics quotas, and GPU-recovery implementation are strengths, but need runtime verification.

Required: publish a supported matrix and record results for fractional scaling/mixed-DPI movement, suspend/resume, GPU loss, keyboard layouts/repeat, CJK preedit/commit, bidi selection/copy, clipboard/drag/drop, screen-reader navigation, multi-window focus, and compositor disconnect. Exercise nvim, helix, tmux, less, htop, SSH, and terminal conformance tools. Add screenshots/replay fixtures where repeatable.

Measure release-build startup, idle CPU/RSS, key-to-frame latency, frame time, output throughput, memory under floods, many panes, and long image-heavy sessions. Include closing/reloading plugins and slow or unavailable filesystems. Establish numeric budgets and regression thresholds on a controlled runner.

### 11. P2: Support diagnostics and product scope need release decisions

Evidence: `crates/proteus/src/gui/mod.rs` uses stderr diagnostics; `docs/design/06-ai.md` describes AI/provider plugins absent from the current source inventory.

GUI launches may not leave users with accessible stderr. There is no documented support-bundle workflow or stable/beta release policy. AI remains a substantial unimplemented roadmap item, rather than an existing feature whose production hardening can be assessed here.

Required: bounded local logs, useful error categories, and an explicit diagnostic export containing version, adapter/backend, compositor protocols, and relevant settings while excluding terminal contents, secrets, and private paths by default. Document crash reporting and recovery. Choose either a terminal/mux/file-manager v1 with AI explicitly deferred, or separately budget implementation and privacy/provider acceptance testing for AI.

## Additional hardening to track

- Audit compiled native WASM cache loading (`runtime.rs:984–1018`): ownership/mode checks are present, but path-based checking followed by unsafe deserialization and predictable temporary writes deserve descriptor-based handling and parent-directory validation. Do not claim an unsigned cache is safe to import from another machine.
- Graphics file/shm handling (`proteus-core/src/graphics/media.rs:36–112`) authorizes paths before opening/unlinking them. Descriptor metadata checks are useful but do not establish that the opened object is the authorized inode; opening a swapped FIFO can block before those checks. Include this boundary in filesystem race tests.
- The image decoder pool has a fixed worker count but an unbounded job channel (`graphics/decode.rs:109–146`). Verify queued encoded buffers, cancelled images, and cross-pane quotas bound aggregate memory; add queue admission/cancellation if they do not.
- Clipboard receive workers cap bytes but have no receive deadline (`gui/mod.rs:2757–2763`). A selection owner that keeps a pipe open can strand a worker. Test repeated stalled offers and add cancellation/deadlines.
- Plugin source builds intentionally execute Cargo/build scripts with normal user privileges, as documented in `docs/design/05-scripting.md`. Preserve that explicit trust distinction in user-facing documentation. Require immutable revisions/checksums for shipped remote sources; post-build artifact hashes do not sandbox build scripts.

## Recommended delivery order

1. Make test execution honest and mandatory; fix CI/lint baseline. Resolve runtime-directory security, filesystem races, and host queue bounds.
2. Add hyperlink reclamation, persistence failure tests, bounded layout parsing, flood/soak tests, and filesystem/terminal-parser fuzzing.
3. Produce an installable beta, runtime diagnostics, and a repeatable compositor/GPU/IME/accessibility acceptance matrix. Recruit daily-use testing with recorded failures and regressions.
4. Ship stable only after the chosen scope has acceptance evidence, reproducible artifacts, upgrade/recovery documentation, and no unresolved P1 findings. Treat AI as a separate milestone unless it is explicitly part of that scope.

The original review was read-only; the hardening follow-up above changes application source and adds CI.
