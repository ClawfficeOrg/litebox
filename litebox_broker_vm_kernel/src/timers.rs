// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Broker timers on the kernel clock (the TSC). With no scheduler, alarms
//! fire when the broker runs: on each `BrokerEnter`, and when a wait on a
//! notification ring halts until the earliest deadline
//! ([`KernelTimers::next_deadline`]). A fired alarm republishes its timer's
//! readiness to its association's notification ring.

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::time::Duration;
use litebox_broker_core::readiness::ReadinessRegistration;
use litebox_broker_core::timer::{Alarm, TimerProvider};
use litebox_broker_protocol::readiness::ReadinessFlags;
use litebox_hal::clock::Clock;
use spin::mutex::SpinMutex;

pub(crate) struct KernelTimers {
    clock: Clock,
    /// Every alarm not yet dropped.
    alarms: SpinMutex<Vec<Weak<KernelAlarm>>>,
}

struct KernelAlarm {
    /// On the [`TimerProvider::now`] clock.
    deadline: SpinMutex<Option<Duration>>,
    readiness: ReadinessRegistration,
}

/// Dropping it cancels the alarm.
struct AlarmHandle(Arc<KernelAlarm>);

impl Alarm for AlarmHandle {
    fn set(&self, deadline: Option<Duration>) {
        *self.0.deadline.lock() = deadline;
    }
}

impl KernelTimers {
    pub(crate) fn new(clock: Clock) -> Self {
        Self {
            clock,
            alarms: SpinMutex::new(Vec::new()),
        }
    }

    /// Fires the alarms whose deadlines passed.
    pub(crate) fn fire(&self) {
        let now = self.now();
        let mut due = Vec::new();
        self.alarms.lock().retain(|alarm| {
            let Some(alarm) = alarm.upgrade() else {
                return false;
            };
            let mut deadline = alarm.deadline.lock();
            if deadline.is_some_and(|deadline| deadline <= now) {
                *deadline = None;
                drop(deadline);
                due.push(alarm);
            }
            true
        });
        // Not under the alarms' locks, which setting or dropping one takes.
        for alarm in due {
            let _ = alarm.readiness.republish(ReadinessFlags::READ);
        }
    }

    /// The earliest armed deadline, as a [`litebox_hal::clock::now`] value.
    pub(crate) fn next_deadline(&self) -> Option<u64> {
        let earliest = self
            .alarms
            .lock()
            .iter()
            .filter_map(|alarm| *alarm.upgrade()?.deadline.lock())
            .min()?;
        let ticks = earliest.as_nanos() * u128::from(self.clock.khz()) / 1_000_000;
        Some(u64::try_from(ticks).unwrap_or(u64::MAX))
    }
}

impl TimerProvider for KernelTimers {
    /// The TSC's time since it started.
    fn now(&self) -> Duration {
        Duration::from_nanos(self.clock.nanos())
    }

    fn create_alarm(
        &self,
        readiness: ReadinessRegistration,
    ) -> litebox_broker_core::Result<Box<dyn Alarm>> {
        let alarm = Arc::new(KernelAlarm {
            deadline: SpinMutex::new(None),
            readiness,
        });
        self.alarms.lock().push(Arc::downgrade(&alarm));
        Ok(Box::new(AlarmHandle(alarm)))
    }
}

/// `deadline` and `other`'s earlier, either being `None` for none.
pub(crate) fn earliest(deadline: Option<u64>, other: Option<u64>) -> Option<u64> {
    match (deadline, other) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}
