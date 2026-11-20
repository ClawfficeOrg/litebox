# Syscall observer (for Chiton)

ClawfficeOrg fork, branch `chiton-syscall-observer`. Lets an embedder see every
guest syscall the Linux shim dispatches, with its arguments, return value and
timing. Off by default; when unset, the dispatch path pays one branch per
syscall and builds no events.

## Registration

On the Windows runner (what Chiton uses):

```rust
use std::sync::Arc;
use litebox_runner_linux_on_windows_userland::{
    CliArgs, SyscallEvent, SyscallObserver, run_with_observer,
};

struct Sink(std::sync::mpsc::SyncSender<(SyscallEvent, Option<i64>)>);

impl SyscallObserver for Sink {
    fn on_enter(&self, ev: &SyscallEvent) {
        let _ = self.0.try_send((*ev, None));
    }
    fn on_exit(&self, ev: &SyscallEvent, ret: i64) {
        let _ = self.0.try_send((*ev, Some(ret)));
    }
}

// In the process that becomes the runner (e.g. Chiton re-executing itself):
run_with_observer(cli_args, Some(Arc::new(Sink(tx))))?;
```

`run(cli_args)` is unchanged and equals `run_with_observer(cli_args, None)`.
Note that `run_with_observer`, like `run`, installs a global tracing subscriber
and ends the process with the guest's exit code, so call it in a dedicated
child process and forward events to the parent (pipe, socket) from the
observer's sink.

At the shim level (any runner):

```rust
let shim = litebox_shim_linux::LinuxShimBuilder::new(platform)
    .with_syscall_observer(observer) // Arc<dyn SyscallObserver>
    .build();
```

## Trait

```rust
pub trait SyscallObserver: Send + Sync {
    fn on_enter(&self, event: &SyscallEvent) {}          // default no-op
    fn on_exit(&self, event: &SyscallEvent, ret: i64) {}  // default no-op
}
```

- Called synchronously on the guest thread that made the syscall. Do not
  block, do not call back into the shim; push the event somewhere and return.
- Per thread, `on_enter` and `on_exit` alternate strictly. Threads interleave;
  pair by `tid`.
- `exit` / `exit_group` report an exit with `ret = 0` that the guest never
  sees. A successful `execve` reports its exit after the new image is set up.

## Event schema

| Field       | Type            | Meaning |
|-------------|-----------------|---------|
| `pid`       | `i32`           | Guest process ID of the caller |
| `tid`       | `i32`           | Guest thread ID of the caller |
| `nr`        | `u64`           | Raw Linux x86-64 syscall number (`orig_rax`) |
| `args`      | `[u64; 6]`      | `rdi, rsi, rdx, r10, r8, r9`, raw. Pointers are guest addresses; do not dereference |
| `timestamp` | `Duration`      | Monotonic time since the shim was built. Entry and exit each carry their own |
| `ret`       | `i64` (exit only) | Value returned to the guest. `-4095..=-1` means `-errno` |

`SyscallEvent::name()` returns the Linux name (`"openat"`) via a static table,
or `None` for unknown numbers. The exit event repeats the entry's `pid`, `tid`,
`nr` and `args`. Latency is `exit.timestamp - enter.timestamp`.

If Chiton forwards events over a pipe, a suggested line-JSON shape is:

```json
{"ph":"enter","pid":1,"tid":1,"nr":257,"name":"openat","args":[18446744073709551516,140737488347136,0,0,0,0],"t_ns":1234567}
{"ph":"exit","pid":1,"tid":1,"nr":257,"name":"openat","ret":3,"t_ns":1240012}
```

The fork itself does not write this format; it is a convention for Chiton's
own sink.

## Scope and limits

- x86-64 only (the shim's dispatch reads `orig_rax`; other arches compile
  without the hook).
- Only syscalls that reach the shim's dispatcher are observed. Exceptions,
  interrupts and page faults are not.
- Arguments are not decoded beyond the name; strings and structs behind
  pointers are not read.
