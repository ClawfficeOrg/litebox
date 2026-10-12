// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Test client for standalone programs: runs those of the payload's
//! `programs.json` in order, and checks how each ends. `programs.json` is an
//! array of runs:
//! - `argv`: `argv[0]` is the program's absolute path (e.g., in
//!   `rootfs.tar`).
//! - `envp`: `KEY=VALUE` strings (default none).
//! - `expect_exit_code` (default 0), or `expect_signal` for a signal death.
//! - `expect_stdout`, `expect_stderr`: the exact output, if given; output
//!   beyond what is recorded fails the check.
//! - `min_reflected_syscalls`: default 0; guest syscalls the runner did not
//!   take directly, so the kernel reflected them.
//!
//! Output also goes to the console, as it would without this client.

use crate::service::Service;
use crate::service::program::Programs;
use alloc::ffi::CString;
use alloc::string::String;
use alloc::vec::Vec;
use litebox_broker_core::stdio::StdioOutputStream;
use litebox_broker_vm_kernel::stdio::Console;
use litebox_common_vm_abi::program::{ExitStatus, Program};
use serde::Deserialize;
use spin::Mutex;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    argv: Vec<String>,
    #[serde(default)]
    envp: Vec<String>,
    expect_exit_code: Option<u32>,
    expect_signal: Option<u32>,
    expect_stdout: Option<String>,
    expect_stderr: Option<String>,
    #[serde(default)]
    min_reflected_syscalls: u64,
}

/// Keeps at most this much of each stream per run; the console gets all.
const MAX_RECORDED: usize = 64 * 1024;

/// What a stream wrote, up to [`MAX_RECORDED`] bytes.
#[derive(Default)]
struct Recorded {
    bytes: Vec<u8>,
    /// The stream wrote more than `bytes`.
    truncated: bool,
}

/// A [`Console`] that records what each output stream wrote to `device`.
pub struct Output {
    device: &'static dyn Console,
    /// Standard output, then standard error.
    recorded: Mutex<[Recorded; 2]>,
}

impl Output {
    pub fn new(device: &'static dyn Console) -> Self {
        Self {
            device,
            recorded: Mutex::new(Default::default()),
        }
    }

    /// Standard output and standard error since the last call.
    fn take(&self) -> [Recorded; 2] {
        core::mem::take(&mut *self.recorded.lock())
    }
}

impl Console for Output {
    fn read(&self, output: &mut [u8]) -> Option<usize> {
        self.device.read(output)
    }

    fn has_input(&self) -> bool {
        self.device.has_input()
    }

    /// Records what the device took.
    fn write(&self, stream: StdioOutputStream, input: &[u8]) -> usize {
        let written = self.device.write(stream, input);
        let index = match stream {
            StdioOutputStream::Stdout => 0,
            StdioOutputStream::Stderr => 1,
        };
        let recorded = &mut self.recorded.lock()[index];
        let room = MAX_RECORDED.saturating_sub(recorded.bytes.len());
        let kept = written.min(room);
        recorded.bytes.extend_from_slice(&input[..kept]);
        recorded.truncated |= kept < written;
        written
    }
}

/// # Panics
///
/// On a malformed `programs.json`, or any unexpected end of a run.
pub fn run(json: &[u8], service: &mut Programs, output: &Output) {
    let runs: Vec<Run> = serde_json::from_slice(json).expect("malformed programs.json");
    assert!(!runs.is_empty(), "programs.json has no runs");
    let strings = |strings: Vec<String>| {
        strings
            .into_iter()
            .map(|string| CString::new(string).expect("programs.json strings have no NUL"))
            .collect::<Vec<_>>()
    };
    for (index, run) in runs.into_iter().enumerate() {
        let path = run.argv.first().expect("a run has an argv").clone();
        litebox_util_log::info!(index:% = index, path:% = path; "running");
        let expected = match (run.expect_exit_code, run.expect_signal) {
            (code, None) => ExitStatus::Exited {
                code: code.unwrap_or(0),
            },
            (None, Some(signal)) => ExitStatus::Signaled { signal },
            (Some(_), Some(_)) => {
                panic!("run {index}: `expect_exit_code` and `expect_signal` exclude each other")
            }
        };
        output.take();
        let reply = service.call(Program {
            path: path.clone(),
            argv: strings(run.argv),
            envp: strings(run.envp),
        });
        let [stdout, stderr] = output.take();
        let status = reply
            .status
            .unwrap_or_else(|error| panic!("run {index} ({path}) did not run: {error:?}"));
        assert_eq!(status, expected, "run {index} ({path}): exit status");
        for (what, expected, actual) in [
            ("stdout", run.expect_stdout, stdout),
            ("stderr", run.expect_stderr, stderr),
        ] {
            if let Some(expected) = expected {
                assert!(
                    !actual.truncated,
                    "run {index} ({path}): {what} exceeded the {MAX_RECORDED} bytes recorded, \
                     so it cannot be checked exactly"
                );
                assert!(
                    expected.as_bytes() == actual.bytes,
                    "run {index} ({path}): {what} was {:?}, expected {expected:?}",
                    String::from_utf8_lossy(&actual.bytes)
                );
            }
        }
        assert!(
            reply.reflected_syscalls >= run.min_reflected_syscalls,
            "run {index} ({path}): {} reflected syscalls, expected at least {}",
            reply.reflected_syscalls,
            run.min_reflected_syscalls
        );
        litebox_util_log::info!(
            index:% = index,
            status:? = status,
            reflected_syscalls:% = reply.reflected_syscalls;
            "run passed"
        );
    }
}
