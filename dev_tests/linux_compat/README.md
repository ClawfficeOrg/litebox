# Linux compatibility baseline

Compare a pinned subset of the [gVisor Linux syscall tests](https://github.com/google/gvisor/tree/master/test/syscalls/linux)
against native Linux and LiteBox. This measures correctness, not performance or
the number of syscall names implemented.

The initial scope is **x86-64 Linux with glibc**, using the out-of-process
`litebox-broker-userland` and `litebox_runner_linux_userland`. The manifest
selects 20 individual cases from `dup_test`, `eventfd_test`, and `brk_test`.
Every case discovered in those binaries must be selected or explicitly excluded
with a reason; a changed inventory is an infrastructure error.

The three exclusions are a test that modifies the host-wide `nr_open` sysctl
and two tests with gVisor-only expectations. Missing LiteBox functionality is
not an exclusion. In particular, the `O_PATH`, resource-limit, threading, and
kernel-version-dependent eventfd cases remain selected.

## Prerequisites and execution

Run in a disposable Linux VM, **as a non-root user**. The manual GitHub Actions
workflow uses a fresh Ubuntu VM. Native tests are real host programs: the harness
does not sandbox them, and must not be used to run arbitrary privileged or
system-mutating tests on a development machine. The checked-in subset is
unprivileged and uses only per-process state and temporary files.

Required tools:

- Python 3.11 or newer (standard library only).
- Git, the repository's Rust toolchain, and the normal LiteBox build prerequisites.
- A C++ compiler (`g++`), glibc development files, `ldd`, and `getconf`.
- Bazel **8.8.1**, matching the pinned upstream `.bazelversion`, or Bazelisk.
- Network access for the initial gVisor checkout and build dependencies.

From the repository root:

```sh
python3 dev_tests/linux_compat/run.py

# An explicitly installed Bazel/Bazelisk binary, fewer build jobs, or a longer timeout:
python3 dev_tests/linux_compat/run.py --bazel /path/to/bazel --jobs 2 --timeout 60

# Harness tests only: no downloads, Bazel, or Rust build:
python3 -m unittest discover -s dev_tests/linux_compat -p 'test_*.py' -v
```

The comparison command fetches the exact commit in `gvisor.json`, builds only
the three upstream C++/GoogleTest targets, and builds the existing LiteBox
packager, broker, and Linux runner. It does not run gVisor's `runsc` harness.
Ordinary Cargo builds and tests do not invoke this tooling.

Persistent generated files stay under `target/linux-compat/` by default:

```text
gvisor/                  # Unmodified, pinned upstream checkout and Bazel symlinks
bazel/                   # Bazel outputs and dependencies (batch mode; no daemon)
bazelisk/                # Bazelisk downloads, when used
litebox/                 # Isolated Cargo target directory
run-<unique-id>/
    setup/               # Build/version logs and commands
    <binary>/
        listing/         # Complete native GoogleTest inventory
        packaging/       # Packager logs
        rootfs.tar
        <index>-<case>/
            native/      # stdout.log, stderr.log, process.json
            litebox/     # stdout.log, stderr.log, process.json
    results.json
    summary.md
```

Use `--work-dir PATH` for a different artifact/cache directory. The harness
refuses to overwrite a modified or wrong-revision upstream checkout. Use a
new work directory when changing the pinned revision. Upstream sources retain
their [Apache-2.0 license](https://github.com/google/gvisor/blob/master/LICENSE);
they are not vendored into LiteBox.

## Execution and reporting

Each case runs in a new native process and a new broker/runner instance, even
after a preceding case fails. Both receive explicit GoogleTest filters, a
fixed umask, noninteractive stdin, and a minimal environment. Inherited
`GTEST_*`, `TEST_ON_GVISOR`, and other gVisor-specific settings are not forwarded.
The original binary runs natively; the packager rewrites the corresponding
guest binary, libraries, and ELF runfiles/helpers. Other runfiles are included
unchanged. Native and guest use separate fresh temporary storage; the guest's
temporary directory is `/tmp` in the broker-owned filesystem.
Native scratch files and host-side broker sockets use short, private directories
under host `/tmp`, removed after each run. This avoids Unix socket path-length
limits when the repository or artifact path is long.

Each process has a private process group. Timeout, interruption, and normal
leader exit all trigger cleanup of remaining group members. This is lifecycle
management, not containment of tests that deliberately create new sessions.
Do not add such tests without providing stronger isolation.

The native user ID and inherited file-descriptor limit are recorded. LiteBox
uses its normal guest identity (UID/GID 1000) and virtual resource limits; this
is not a claim that every host kernel or configuration is equivalent.
Recordings also include kernel, compiler, glibc, Rust, Python and Bazel versions,
the source revisions and worktree state, and hashes of the harness, manifest,
test binaries, LiteBox tools, and packaged filesystems.

| Status | Meaning |
|---|---|
| `pass` | Exactly the requested test ran, emitted its success marker, and exited successfully. |
| `failure` | The requested test reported failed assertions and exited unsuccessfully. |
| `crash` | Signal, Rust panic, or abnormal exit while/after executing the test. |
| `timeout` | The per-process deadline expired; the process group was terminated. |
| `skip` | The requested test explicitly reported a GoogleTest skip. |
| `infrastructure_error` | Build, packaging, launch, inventory, or execution-protocol failure, including zero tests running. |

A crash or timeout can happen before the first assertion; `test_started` records
whether the requested GoogleTest case was entered. Pre-test failures without a
recognizable panic or signal remain infrastructure errors rather than being
guessed to be syscall failures. Raw logs are always retained.

The report shows both outcomes for every selected case, including native
failures and skips. Its compatibility count is **LiteBox passes among
native-pass cases**, with the denominator, all status counts, and explicit
exclusions also reported. Skips never count as passes; a native failure is not
evidence of a LiteBox incompatibility. No overall percentage is claimed.

Exit codes are `0` for no failures (including matching skips), `1` for test
failures/crashes/timeouts or a LiteBox skip of a native-passing case, `2` for
infrastructure errors or no executed cases, and `130` for interruption.
The report is updated after each completed pair and on handled failure.

At the initial LiteBox baseline (`254c91c8a`), all 20 cases passed natively;
LiteBox had 10 passes, 9 assertion failures, and 1 skip. All nine failures
occurred in temporary-file fixture setup because `chmod` returned `ENOSYS`:
they are not evidence of nine independent `dup` defects. The eventfd skip
followed the guest's reported kernel version, despite passing on native Linux.
These observations are environment-specific; use the recorded logs and
metadata rather than treating the counts as universal.

## CI

Run **Linux compatibility baseline** using `workflow_dispatch`. It runs the
harness tests, executes the comparison, and uploads the report and logs even
when comparison fails. Expected compatibility gaps are not hidden by
`continue-on-error`; the workflow is manual and is not a required PR gate.
Upstream caches, downloaded sources, Cargo build trees, and rootfs images are
not uploaded. A stable required subset and broader scheduled discovery can be
introduced after this baseline is understood.

GitHub requires a `workflow_dispatch` workflow to exist on the repository's
default branch before it can be dispatched. Once registered there, select the
desired branch/ref for the run.
