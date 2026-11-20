// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Opt-in syscall observation.
//!
//! A [`SyscallObserver`] registered with [`LinuxShimBuilder::with_syscall_observer`] is told about
//! every guest syscall the shim dispatches: once on entry, before the shim acts on it, and once on
//! exit, with the value that is about to be written back into the guest's return register.
//!
//! The hook is designed so that its cost when nobody is watching is a single, well-predicted
//! branch on an `Option` that never changes after the shim is built. Nothing is allocated, no
//! clock is read and no event is built unless an observer is present. This matters because the
//! dispatch path runs on every guest syscall.
//!
//! Observers are called synchronously on the guest thread that issued the syscall, so they must
//! be cheap and must not block: hand the event to a channel, a ring buffer or a buffered writer
//! and return. An observer must not call back into the shim.
//!
//! [`LinuxShimBuilder::with_syscall_observer`]: crate::LinuxShimBuilder::with_syscall_observer

use core::time::Duration;

/// One syscall as seen at the shim's dispatch boundary.
///
/// This is deliberately a plain, `Copy` value with no pointers into guest memory: arguments are
/// the raw register values, so pointer arguments are guest addresses that an observer must not
/// dereference. Decoding (beyond the syscall name, see [`SyscallEvent::name`]) is left to the
/// consumer so that the hot path stays free of allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyscallEvent {
    /// Guest process ID of the calling task.
    pub pid: i32,
    /// Guest thread ID of the calling task.
    pub tid: i32,
    /// Raw Linux syscall number (x86-64 numbering), as found in `orig_rax`.
    pub nr: u64,
    /// The six syscall argument registers, in ABI order (`rdi`, `rsi`, `rdx`, `r10`, `r8`,
    /// `r9`). Syscalls with fewer arguments leave the rest as whatever the guest had there.
    pub args: [u64; 6],
    /// Monotonic time since the shim was built, read when this event was created. Entry and exit
    /// events carry separate timestamps, so their difference is the time spent in the shim.
    pub timestamp: Duration,
}

impl SyscallEvent {
    /// Returns the Linux name of the syscall (for example `"openat"`), or `None` if the number is
    /// unknown on this architecture.
    ///
    /// This is a table lookup returning a `&'static str`; it does not allocate.
    #[must_use]
    pub fn name(&self) -> Option<&'static str> {
        let nr = usize::try_from(self.nr).ok()?;
        ::syscalls::Sysno::new(nr).map(|s| s.name())
    }
}

/// Receives syscall entry and exit notifications from the Linux shim.
///
/// Both methods default to doing nothing, so an implementation only overrides what it needs.
/// For every syscall the shim dispatches, `on_enter` is called first and `on_exit` second, on the
/// same thread, with no other syscall of that thread in between. Syscalls from different guest
/// threads may interleave; use [`SyscallEvent::tid`] to pair them.
///
/// Two cases do not follow the plain enter/exit shape: `exit` and `exit_group` report an exit with
/// a return value of `0` even though the guest never sees it, and a successful `execve` reports
/// its exit after the new image has been set up.
pub trait SyscallObserver: Send + Sync {
    /// Called before the shim acts on a syscall. `event.timestamp` is the entry time.
    fn on_enter(&self, event: &SyscallEvent) {
        let _ = event;
    }

    /// Called after the shim has handled a syscall.
    ///
    /// `event` has the same `pid`, `tid`, `nr` and `args` as the matching entry event, with
    /// `timestamp` set to the exit time. `ret` is the value returned to the guest, as a signed
    /// integer: negative values in `-4095..=-1` are `-errno`, as in the Linux ABI.
    fn on_exit(&self, event: &SyscallEvent, ret: i64) {
        let _ = (event, ret);
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "tests run on x86-64 and move raw register values between u64, usize and i32"
)]
mod tests {
    extern crate std;

    use super::{SyscallEvent, SyscallObserver};
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use litebox::fs::Mode;
    use litebox_common_linux::PtRegs;
    use std::sync::Mutex;

    const TEST_TAR_FILE: &[u8] = include_bytes!("../../litebox/src/fs/test.tar");

    const NR_CLOSE: u64 = 3;
    const NR_GETPID: u64 = 39;

    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Enter(SyscallEvent),
        Exit(SyscallEvent, i64),
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Seen>>);

    impl SyscallObserver for Recorder {
        fn on_enter(&self, event: &SyscallEvent) {
            self.0.lock().unwrap().push(Seen::Enter(*event));
        }
        fn on_exit(&self, event: &SyscallEvent, ret: i64) {
            self.0.lock().unwrap().push(Seen::Exit(*event, ret));
        }
    }

    /// Builds a task without loading a guest, optionally with an observer registered.
    fn task_with(
        observer: Option<Arc<dyn SyscallObserver>>,
    ) -> crate::Task<crate::syscalls::tests::TestPlatform> {
        let platform = crate::syscalls::tests::test_platform(None);
        let mut builder = crate::LinuxShimBuilder::new(platform);
        if let Some(obs) = observer {
            builder = builder.with_syscall_observer(obs);
        }
        let in_mem =
            litebox::fs::in_mem::InMem::new_initialized(crate::default_fs_entries().chain([(
                "/",
                litebox::fs::in_mem::InitialNode::Directory {
                    mode: Mode::RWXU | Mode::RWXG | Mode::RWXO,
                    owner: litebox::fs::UserInfo::ROOT,
                },
            )]));
        let fs = Arc::new(builder.default_fs(in_mem, TEST_TAR_FILE.into()));
        builder.build().0.new_test_task(fs)
    }

    /// A synthetic dispatch: set up registers as the guest would and enter the shim's syscall
    /// handler directly, with no guest code running.
    fn dispatch(
        task: &crate::Task<crate::syscalls::tests::TestPlatform>,
        nr: u64,
        args: [u64; 6],
    ) -> usize {
        let mut regs = PtRegs {
            orig_rax: nr as usize,
            rdi: args[0] as usize,
            rsi: args[1] as usize,
            rdx: args[2] as usize,
            r10: args[3] as usize,
            r8: args[4] as usize,
            r9: args[5] as usize,
            ..PtRegs::default()
        };
        task.handle_syscall_request(&mut regs);
        regs.rax
    }

    #[test]
    fn observer_sees_enter_exit_pairs_in_order() {
        let recorder = Arc::new(Recorder::default());
        let task = task_with(Some(recorder.clone()));

        let pid_ret = dispatch(&task, NR_GETPID, [0; 6]);
        let bad_fd = u64::from(u32::MAX); // -1 as an i32 fd
        let close_ret = dispatch(&task, NR_CLOSE, [bad_fd, 11, 22, 33, 44, 55]);

        let seen = recorder.0.lock().unwrap();
        assert_eq!(seen.len(), 4, "expected two enter/exit pairs, got {seen:?}");

        let (Seen::Enter(e0), Seen::Exit(x0, r0)) = (&seen[0], &seen[1]) else {
            panic!("first pair out of order: {seen:?}");
        };
        assert_eq!(e0.nr, NR_GETPID);
        assert_eq!(e0.name(), Some("getpid"));
        assert_eq!((e0.pid, e0.tid), (task.pid, task.tid));
        assert_eq!(
            (x0.nr, x0.pid, x0.tid, x0.args),
            (e0.nr, e0.pid, e0.tid, e0.args)
        );
        assert_eq!(*r0, i64::from(task.pid));
        assert_eq!(pid_ret, task.pid as usize);
        assert!(x0.timestamp >= e0.timestamp);

        let (Seen::Enter(e1), Seen::Exit(x1, r1)) = (&seen[2], &seen[3]) else {
            panic!("second pair out of order: {seen:?}");
        };
        assert_eq!(e1.nr, NR_CLOSE);
        assert_eq!(e1.name(), Some("close"));
        assert_eq!(e1.args, [bad_fd, 11, 22, 33, 44, 55]);
        assert_eq!(x1.args, e1.args);
        assert_eq!(*r1, -9, "close(-1) should report -EBADF");
        assert_eq!(close_ret as isize as i64, *r1);
        assert!(e1.timestamp >= x0.timestamp);
    }

    #[test]
    fn nothing_emitted_when_unset() {
        // A recorder that exists but is never registered must see nothing, and the shim must
        // carry no observer at all.
        let recorder = Arc::new(Recorder::default());
        let task = task_with(None);
        assert!(task.global.syscall_observer.is_none());

        let pid_ret = dispatch(&task, NR_GETPID, [0; 6]);
        assert_eq!(pid_ret, task.pid as usize);
        assert!(recorder.0.lock().unwrap().is_empty());
    }

    #[test]
    fn unknown_syscall_number_has_no_name() {
        let event = SyscallEvent {
            pid: 1,
            tid: 1,
            nr: 100_000,
            args: [0; 6],
            timestamp: core::time::Duration::ZERO,
        };
        assert_eq!(event.name(), None);
    }
}
