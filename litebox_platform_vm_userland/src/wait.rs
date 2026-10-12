// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Waits and timers. With one thread, nothing else in the runner can change
//! a waiter's word, so a wait ends only by something outside it: an event,
//! which the runner's [`EventSource`] handles (e.g., broker notifications
//! waking pollers), a [`Timer`]'s deadline, or the wait's own timeout. A
//! wakeup need not change the word ([`RawMutex`] wakeups may be spurious), so
//! after one the shim rechecks its conditions, including pending signals,
//! where expired timers show up ([`take_pending_signals`]).
//!
//! Without an event source, waits only sleep until a deadline, on the
//! kernel's deadline wait.
//!
//! Events are also polled, without waiting, whenever the guest enters the
//! shim (`guest_entered`), so that nonblocking checks (e.g., `epoll_wait`
//! with no timeout) see them. With no preemption, the kernel publishes due
//! broker timers only when the runner enters it, so at most every
//! [`REFRESH_INTERVAL_MS`] such a poll enters it too.
//!
//! Timers fire at those rechecks, i.e., at a syscall or a wait; a guest that
//! makes neither does not see them until it does.
//!
//! [`take_pending_signals`]: litebox::platform::SignalProvider::take_pending_signals

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use core::time::Duration;
use litebox_common_linux::signal::Signal;
use litebox_common_vm_abi::{BrokerEnterRequest, StartupInfo, Status};
use litebox_platform::sync::{ImmediatelyWokenUp, RawMutex as RawMutexTrait, UnblockedOrTimedOut};

use crate::{VmUserland, kcall};

/// What can end a wait, besides deadlines.
pub trait EventSource: Sync {
    /// Waits until an event may have happened, and handles what did (e.g.,
    /// dispatches broker notifications), or until `deadline`, a TSC value,
    /// passes. Without a deadline: false, and no wait, if no event could ever
    /// happen.
    fn wait(&self, deadline: Option<u64>) -> bool;

    /// Handles the events that already happened, without waiting; with
    /// `refresh`, first has their sources report what is due (e.g., expired
    /// broker timers), which may cost a kernel entry.
    fn poll(&self, refresh: bool);
}

/// How often, at most, `guest_entered` refreshes the event source.
pub const REFRESH_INTERVAL_MS: u64 = 1;

/// What waits and timers need, which get no platform reference; set by
/// [`VmUserland::new`] and [`VmUserland::set_event_source`].
struct Runtime {
    tsc_khz: AtomicU64,
    /// [`StartupInfo::broker_control_ring`]'s start.
    control_ring: AtomicU64,
    events: spin::Once<&'static dyn EventSource>,
    /// When `guest_entered` last refreshed the event source (TSC).
    last_refresh: AtomicU64,
    /// Every timer not yet dropped.
    timers: spin::Mutex<Vec<Weak<TimerState>>>,
}

static RUNTIME: Runtime = Runtime {
    tsc_khz: AtomicU64::new(0),
    control_ring: AtomicU64::new(0),
    events: spin::Once::new(),
    last_refresh: AtomicU64::new(0),
    timers: spin::Mutex::new(Vec::new()),
};

/// Polls events as the guest enters the shim (see the [module docs](self)).
pub(crate) fn guest_entered() {
    let Some(source) = RUNTIME.events.get() else {
        return;
    };
    let now = rdtsc();
    let interval = RUNTIME
        .tsc_khz
        .load(Ordering::Relaxed)
        .saturating_mul(REFRESH_INTERVAL_MS);
    let refresh = now.saturating_sub(RUNTIME.last_refresh.load(Ordering::Relaxed)) >= interval;
    if refresh {
        RUNTIME.last_refresh.store(now, Ordering::Relaxed);
    }
    source.poll(refresh);
}

pub(crate) fn init(info: &StartupInfo) {
    RUNTIME.tsc_khz.store(info.tsc_khz, Ordering::Relaxed);
    RUNTIME
        .control_ring
        .store(info.broker_control_ring.start, Ordering::Relaxed);
}

impl VmUserland {
    /// Lets waits end at `source`'s events. At most once.
    ///
    /// # Panics
    ///
    /// On a second call.
    pub fn set_event_source(&self, source: &'static dyn EventSource) {
        let mut set = false;
        RUNTIME.events.call_once(|| {
            set = true;
            source
        });
        assert!(set, "the event source is already set");
    }
}

pub(crate) fn rdtsc() -> u64 {
    // Safety: RDTSC has no side effects; the kernel allows it in ring 3.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// The earlier of two deadlines, either being `None` for none.
fn earliest(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl Runtime {
    /// The TSC value `time` from now.
    fn deadline(&self, time: Duration) -> u64 {
        let khz = u128::from(self.tsc_khz.load(Ordering::Relaxed));
        let ticks =
            u64::try_from(time.as_nanos().saturating_mul(khz) / 1_000_000).unwrap_or(u64::MAX);
        rdtsc().saturating_add(ticks)
    }

    /// Waits until an event, `deadline`, or the earliest timer's deadline;
    /// false if nothing could end the wait.
    fn wait(&self, deadline: Option<u64>) -> bool {
        let deadline = earliest(deadline, self.next_timer());
        if deadline.is_some_and(|deadline| rdtsc() >= deadline) {
            return true;
        }
        match (self.events.get(), deadline) {
            (Some(source), deadline) => source.wait(deadline),
            (None, Some(deadline)) => {
                self.sleep_until(deadline);
                true
            }
            (None, None) => false,
        }
    }

    /// Halts until `deadline` with the kernel's deadline wait (`BrokerEnter`'s
    /// Wait), on a control-ring word that only the kernel's serving changes:
    /// the request ring's consumer epoch.
    ///
    /// # Panics
    ///
    /// If the kernel refuses the wait, e.g., before the broker handshake.
    fn sleep_until(&self, deadline: u64) {
        use litebox_broker_transport::control_ring::ControlRingDirection;
        let offset = ControlRingDirection::Requests.consumer_epoch_offset();
        let word = usize::try_from(self.control_ring.load(Ordering::Relaxed))
            .expect("x86-64 addresses fit usize")
            + offset;
        while rdtsc() < deadline {
            // Safety: the control ring is mapped for the process's lifetime,
            // and its words are aligned `u32`s.
            let current = unsafe { (*(word as *const AtomicU32)).load(Ordering::Acquire) };
            match kcall::call(&BrokerEnterRequest::wait(offset as u64, current, deadline)) {
                // The word changed (served requests), or the deadline passed.
                Ok(()) | Err(Status::TimedOut) => {}
                Err(status) => panic!("timed wait: {status:?}"),
            }
        }
    }

    /// The earliest armed timer's deadline.
    fn next_timer(&self) -> Option<u64> {
        self.timers
            .lock()
            .iter()
            .filter_map(
                |timer| match timer.upgrade()?.deadline.load(Ordering::Relaxed) {
                    DISARMED => None,
                    deadline => Some(deadline),
                },
            )
            .min()
    }
}

/// See the [module docs](self).
pub struct RawMutex {
    inner: AtomicU32,
}

impl RawMutexTrait for RawMutex {
    const INIT: Self = Self {
        inner: AtomicU32::new(0),
    };

    fn underlying_atomic(&self) -> &AtomicU32 {
        &self.inner
    }

    fn wake_many(&self, _n: usize) -> usize {
        0
    }

    /// # Panics
    ///
    /// If nothing could end the wait, which would deadlock.
    fn block(&self, val: u32) -> Result<(), ImmediatelyWokenUp> {
        if self.inner.load(Ordering::Relaxed) != val {
            return Err(ImmediatelyWokenUp);
        }
        assert!(
            RUNTIME.wait(None),
            "a wait that nothing could end in the single-threaded VM userland runner would deadlock"
        );
        Ok(())
    }

    fn block_or_timeout(
        &self,
        val: u32,
        time: Duration,
    ) -> Result<UnblockedOrTimedOut, ImmediatelyWokenUp> {
        if self.inner.load(Ordering::Relaxed) != val {
            return Err(ImmediatelyWokenUp);
        }
        let until = RUNTIME.deadline(time);
        RUNTIME.wait(Some(until));
        Ok(if rdtsc() >= until {
            UnblockedOrTimedOut::TimedOut
        } else {
            UnblockedOrTimedOut::Unblocked
        })
    }
}

const DISARMED: u64 = 0;

struct TimerState {
    signal: Signal,
    /// A TSC value, or [`DISARMED`].
    deadline: AtomicU64,
    /// Expired, but its signal not yet taken: rearming or cancelling the
    /// timer does not lose it.
    expired: AtomicBool,
}

impl TimerState {
    /// Disarms the timer if `deadline` passed by `now`, marking it expired.
    fn expire(&self, deadline: u64, now: u64) {
        if deadline != DISARMED && deadline <= now {
            self.expired.store(true, Ordering::Relaxed);
        }
    }
}

/// A one-shot timer that raises its signal ([`take_pending_signals`]); see the
/// [module docs](self).
///
/// [`take_pending_signals`]: litebox::platform::SignalProvider::take_pending_signals
pub struct Timer(Arc<TimerState>);

impl litebox::platform::TimerHandle for Timer {
    fn set_timer(&self, duration: Duration) {
        let deadline = if duration.is_zero() {
            DISARMED
        } else {
            RUNTIME.deadline(duration).max(DISARMED + 1)
        };
        let previous = self.0.deadline.swap(deadline, Ordering::Relaxed);
        self.0.expire(previous, rdtsc());
    }
}

impl litebox::platform::TimerProvider for VmUserland {
    type TimerHandle = Timer;
    type Signal = Signal;

    fn create_timer(&self, signal: Signal) -> Result<Timer, litebox::platform::TimerCreationError> {
        let timer = Arc::new(TimerState {
            signal,
            deadline: AtomicU64::new(DISARMED),
            expired: AtomicBool::new(false),
        });
        RUNTIME.timers.lock().push(Arc::downgrade(&timer));
        Ok(Timer(timer))
    }
}

/// The signals of expired [`Timer`]s, which it disarms.
impl litebox::platform::SignalProvider for VmUserland {
    type Signal = Signal;

    fn take_pending_signals(&self, mut f: impl FnMut(Signal)) {
        let now = rdtsc();
        let mut due = Vec::new();
        RUNTIME.timers.lock().retain(|timer| {
            let Some(timer) = timer.upgrade() else {
                return false;
            };
            let deadline = timer.deadline.load(Ordering::Relaxed);
            if deadline != DISARMED && deadline <= now {
                timer.deadline.store(DISARMED, Ordering::Relaxed);
            }
            timer.expire(deadline, now);
            if timer.expired.swap(false, Ordering::Relaxed) {
                due.push(timer.signal);
            }
            true
        });
        // Not under the timers' lock, which the shim may need.
        due.into_iter().for_each(&mut f);
    }
}
