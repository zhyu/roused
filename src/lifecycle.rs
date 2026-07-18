use crate::wake::WakeTarget;
use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time::{MissedTickBehavior, interval, timeout};

const LAUNCHCTL_PATH: &str = "/bin/launchctl";
const IDLE_SCAN_INTERVAL: Duration = Duration::from_millis(100);
pub(crate) const STOP_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const STOP_CHECK_RETRY: Duration = Duration::from_secs(30);

pub(crate) struct ServiceLifecycle {
    launchd_label: String,
    user_id: u32,
    idle_timeout: Duration,
    can_stop_command: Option<Vec<String>>,
    wake_target: Arc<WakeTarget>,
    state: Mutex<LifecycleState>,
}

#[derive(Debug)]
struct LifecycleState {
    in_flight: usize,
    last_completed: Instant,
    idle_since: Instant,
    generation: u64,
    attempt_in_progress: bool,
    last_check_finished: Option<Instant>,
    stop_attempted_generation: Option<u64>,
}

pub(crate) struct ServiceLease {
    lifecycle: Arc<ServiceLifecycle>,
    released: bool,
}

#[derive(Clone)]
pub(crate) struct IdleMonitor {
    services: Vec<Arc<ServiceLifecycle>>,
}

#[derive(Debug)]
struct StopAttempt {
    generation: u64,
    can_stop_command: Option<Vec<String>>,
}

#[derive(Debug, Eq, PartialEq)]
enum StopCheckOutcome {
    Allowed,
    Vetoed,
    Failed,
    TimedOut,
}

#[derive(Debug, Eq, PartialEq)]
struct StopCheckSpec {
    program: String,
    arguments: Vec<String>,
    timeout: Duration,
}

#[derive(Debug, Eq, PartialEq)]
struct StopCommandSpec {
    program: &'static str,
    arguments: [String; 3],
}

impl ServiceLifecycle {
    pub(crate) fn new(
        launchd_label: String,
        user_id: u32,
        idle_timeout: Duration,
        can_stop_command: Option<Vec<String>>,
        wake_target: Arc<WakeTarget>,
    ) -> Arc<Self> {
        Self::new_at(
            launchd_label,
            user_id,
            idle_timeout,
            can_stop_command,
            wake_target,
            Instant::now(),
        )
    }

    fn new_at(
        launchd_label: String,
        user_id: u32,
        idle_timeout: Duration,
        can_stop_command: Option<Vec<String>>,
        wake_target: Arc<WakeTarget>,
        now: Instant,
    ) -> Arc<Self> {
        Arc::new(Self {
            launchd_label,
            user_id,
            idle_timeout,
            can_stop_command,
            wake_target,
            state: Mutex::new(LifecycleState {
                in_flight: 0,
                last_completed: now,
                idle_since: now,
                generation: 0,
                attempt_in_progress: false,
                last_check_finished: None,
                stop_attempted_generation: None,
            }),
        })
    }

    pub(crate) fn note_request_arrival(&self) {
        self.note_request_arrival_at(Instant::now());
    }

    fn note_request_arrival_at(&self, now: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.idle_since = now;
        state.generation = state.generation.wrapping_add(1);
        state.last_check_finished = None;
        state.stop_attempted_generation = None;
    }

    pub(crate) fn acquire(self: &Arc<Self>) -> ServiceLease {
        self.acquire_at(Instant::now())
    }

    fn acquire_at(self: &Arc<Self>, _now: Instant) -> ServiceLease {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.in_flight = state
            .in_flight
            .checked_add(1)
            .expect("service in-flight count overflowed");
        state.generation = state.generation.wrapping_add(1);
        state.last_check_finished = None;
        state.stop_attempted_generation = None;
        drop(state);
        ServiceLease {
            lifecycle: Arc::clone(self),
            released: false,
        }
    }

    fn release_at(&self, now: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.in_flight = state
            .in_flight
            .checked_sub(1)
            .expect("service lease released without an in-flight request");
        state.last_completed = now;
        state.idle_since = now;
        state.generation = state.generation.wrapping_add(1);
        state.last_check_finished = None;
        state.stop_attempted_generation = None;
    }

    fn begin_stop_attempt_at(&self, now: Instant) -> Option<StopAttempt> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.in_flight != 0
            || state.attempt_in_progress
            || now.saturating_duration_since(state.idle_since) < self.idle_timeout
            || state.stop_attempted_generation == Some(state.generation)
            || state
                .last_check_finished
                .is_some_and(|finished| now.saturating_duration_since(finished) < STOP_CHECK_RETRY)
        {
            return None;
        }

        state.attempt_in_progress = true;
        Some(StopAttempt {
            generation: state.generation,
            can_stop_command: self.can_stop_command.clone(),
        })
    }

    fn finish_vetoed_attempt_at(&self, attempt: &StopAttempt, now: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.attempt_in_progress = false;
        if state.generation == attempt.generation && state.in_flight == 0 {
            state.last_check_finished = Some(now);
        }
    }

    fn finish_allowed_attempt_at<F>(
        &self,
        attempt: &StopAttempt,
        now: Instant,
        start_stop: F,
    ) -> bool
    where
        F: FnOnce() -> bool,
    {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.attempt_in_progress = false;
        if state.generation != attempt.generation
            || state.in_flight != 0
            || now.saturating_duration_since(state.idle_since) < self.idle_timeout
        {
            return false;
        }

        if start_stop() {
            // This separate launch-state lock is always taken after the lifecycle
            // lock, matching the request path's lifecycle-then-wake ordering.
            self.wake_target.allow_launch_after_stop();
            state.stop_attempted_generation = Some(state.generation);
            true
        } else {
            state.last_check_finished = Some(now);
            false
        }
    }

    fn stop_check_spec(command: Vec<String>, check_timeout: Duration) -> StopCheckSpec {
        let mut command = command.into_iter();
        StopCheckSpec {
            program: command
                .next()
                .expect("validated stop-check command is nonempty"),
            arguments: command.collect(),
            timeout: check_timeout,
        }
    }

    fn stop_command_spec(&self) -> StopCommandSpec {
        StopCommandSpec {
            program: LAUNCHCTL_PATH,
            arguments: [
                "kill".to_owned(),
                "SIGTERM".to_owned(),
                format!("gui/{}/{}", self.user_id, self.launchd_label),
            ],
        }
    }

    async fn run_stop_attempt(self: Arc<Self>, attempt: StopAttempt) {
        let outcome = match attempt.can_stop_command.clone() {
            Some(command) => {
                run_stop_check(Self::stop_check_spec(command, STOP_CHECK_TIMEOUT)).await
            }
            None => StopCheckOutcome::Allowed,
        };

        match outcome {
            StopCheckOutcome::Allowed => {
                let spec = self.stop_command_spec();
                let label = self.launchd_label.clone();
                let stopped = self.finish_allowed_attempt_at(&attempt, Instant::now(), || {
                    start_stop_command(spec, label)
                });
                if !stopped {
                    log::info!(
                        "obsolete stop decision discarded for configured service {}",
                        self.launchd_label
                    );
                }
            }
            StopCheckOutcome::Vetoed => {
                self.finish_vetoed_attempt_at(&attempt, Instant::now());
                log::info!(
                    "stop check vetoed shutdown for configured service {}",
                    self.launchd_label
                );
            }
            StopCheckOutcome::Failed => {
                self.finish_vetoed_attempt_at(&attempt, Instant::now());
                log::warn!(
                    "stop check failed for configured service {}",
                    self.launchd_label
                );
            }
            StopCheckOutcome::TimedOut => {
                self.finish_vetoed_attempt_at(&attempt, Instant::now());
                log::warn!(
                    "stop check timed out for configured service {}",
                    self.launchd_label
                );
            }
        }
    }
}

impl ServiceLease {
    fn release_at(&mut self, now: Instant) {
        if !self.released {
            self.lifecycle.release_at(now);
            self.released = true;
        }
    }
}

impl Drop for ServiceLease {
    fn drop(&mut self) {
        self.release_at(Instant::now());
    }
}

impl IdleMonitor {
    pub(crate) fn new(services: Vec<Arc<ServiceLifecycle>>) -> Self {
        Self { services }
    }

    fn scan_at(&self, now: Instant) {
        for service in &self.services {
            if let Some(attempt) = service.begin_stop_attempt_at(now) {
                tokio::spawn(Arc::clone(service).run_stop_attempt(attempt));
            }
        }
    }
}

#[async_trait]
impl BackgroundService for IdleMonitor {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let mut scan = interval(IDLE_SCAN_INTERVAL);
        scan.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = scan.tick() => self.scan_at(Instant::now()),
            }
        }
    }
}

async fn run_stop_check(spec: StopCheckSpec) -> StopCheckOutcome {
    let mut command = Command::new(spec.program);
    command
        .args(spec.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let Ok(mut child) = command.spawn() else {
        return StopCheckOutcome::Failed;
    };
    match timeout(spec.timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => StopCheckOutcome::Allowed,
        Ok(Ok(_)) => StopCheckOutcome::Vetoed,
        Ok(Err(_)) => StopCheckOutcome::Failed,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            StopCheckOutcome::TimedOut
        }
    }
}

fn start_stop_command(spec: StopCommandSpec, launchd_label: String) -> bool {
    let mut command = Command::new(spec.program);
    command
        .args(spec.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let Ok(mut child) = command.spawn() else {
        log::warn!(
            "launchctl stop failed to start for configured service {}",
            launchd_label
        );
        return false;
    };
    log::info!(
        "launchctl stop started for configured service {}",
        launchd_label
    );
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) if status.success() => log::info!(
                "launchctl stop completed for configured service {}",
                launchd_label
            ),
            _ => log::warn!(
                "launchctl stop failed for configured service {}",
                launchd_label
            ),
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn lifecycle_at(
        now: Instant,
        idle_timeout: Duration,
        command: Option<Vec<String>>,
    ) -> Arc<ServiceLifecycle> {
        let wake_target = WakeTarget::new(
            "127.0.0.1:19003".parse().unwrap(),
            "net.test.lifecycle".to_owned(),
            501,
        );
        ServiceLifecycle::new_at(
            "net.test.lifecycle".to_owned(),
            501,
            idle_timeout,
            command,
            wake_target,
            now,
        )
    }

    #[test]
    fn startup_grace_and_release_accounting_use_explicit_times() {
        let start = Instant::now();
        let lifecycle = lifecycle_at(start, Duration::from_secs(10), None);
        assert!(
            lifecycle
                .begin_stop_attempt_at(start + Duration::from_secs(9))
                .is_none()
        );

        lifecycle.note_request_arrival_at(start + Duration::from_secs(5));
        let mut lease = lifecycle.acquire_at(start + Duration::from_secs(5));
        {
            let state = lifecycle
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            assert_eq!(state.in_flight, 1);
            assert_eq!(state.generation, 2);
        }
        assert!(
            lifecycle
                .begin_stop_attempt_at(start + Duration::from_secs(30))
                .is_none()
        );

        let completed = start + Duration::from_secs(31);
        lease.release_at(completed);
        lease.release_at(completed + Duration::from_secs(1));
        let state = lifecycle
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(state.in_flight, 0);
        assert_eq!(state.last_completed, completed);
        assert_eq!(state.idle_since, completed);
        assert_eq!(state.generation, 3);
    }

    #[test]
    fn veto_retry_boundary_is_thirty_seconds_while_idle() {
        let start = Instant::now();
        let lifecycle = lifecycle_at(start, Duration::from_secs(1), None);
        let first = lifecycle
            .begin_stop_attempt_at(start + Duration::from_secs(1))
            .unwrap();
        let finished = start + Duration::from_secs(2);
        lifecycle.finish_vetoed_attempt_at(&first, finished);

        assert!(
            lifecycle
                .begin_stop_attempt_at(finished + STOP_CHECK_RETRY - Duration::from_nanos(1))
                .is_none()
        );
        assert!(
            lifecycle
                .begin_stop_attempt_at(finished + STOP_CHECK_RETRY)
                .is_some()
        );
    }

    #[test]
    fn activity_during_check_invalidates_atomic_stop() {
        let start = Instant::now();
        let lifecycle = lifecycle_at(start, Duration::from_secs(1), None);
        let attempt = lifecycle
            .begin_stop_attempt_at(start + Duration::from_secs(1))
            .unwrap();
        lifecycle.note_request_arrival_at(start + Duration::from_secs(2));
        let stop_called = AtomicBool::new(false);
        assert!(!lifecycle.finish_allowed_attempt_at(
            &attempt,
            start + Duration::from_secs(3),
            || {
                stop_called.store(true, Ordering::SeqCst);
                true
            }
        ));
        assert!(!stop_called.load(Ordering::SeqCst));
    }

    #[test]
    fn no_checker_allows_idle_stop_and_command_is_fixed() {
        let start = Instant::now();
        let lifecycle = lifecycle_at(start, Duration::from_secs(1), None);
        let attempt = lifecycle
            .begin_stop_attempt_at(start + Duration::from_secs(1))
            .unwrap();
        assert!(attempt.can_stop_command.is_none());
        assert_eq!(
            lifecycle.stop_command_spec(),
            StopCommandSpec {
                program: "/bin/launchctl",
                arguments: [
                    "kill".to_owned(),
                    "SIGTERM".to_owned(),
                    "gui/501/net.test.lifecycle".to_owned(),
                ],
            }
        );
        assert!(lifecycle.finish_allowed_attempt_at(
            &attempt,
            start + Duration::from_secs(1),
            || true
        ));
        assert!(
            lifecycle
                .begin_stop_attempt_at(start + Duration::from_secs(2))
                .is_none()
        );
    }

    #[test]
    fn checker_spec_preserves_direct_argv_and_fixed_timeout() {
        let spec = ServiceLifecycle::stop_check_spec(
            vec![
                "/absolute/checker".to_owned(),
                "literal argument".to_owned(),
                "$(not-a-shell)".to_owned(),
            ],
            STOP_CHECK_TIMEOUT,
        );
        assert_eq!(
            spec,
            StopCheckSpec {
                program: "/absolute/checker".to_owned(),
                arguments: vec!["literal argument".to_owned(), "$(not-a-shell)".to_owned()],
                timeout: Duration::from_secs(5),
            }
        );
    }

    #[tokio::test]
    async fn checker_outcomes_are_conservative_and_bounded() {
        let allowed = run_stop_check(StopCheckSpec {
            program: "/usr/bin/true".to_owned(),
            arguments: Vec::new(),
            timeout: Duration::from_secs(1),
        })
        .await;
        assert_eq!(allowed, StopCheckOutcome::Allowed);

        let vetoed = run_stop_check(StopCheckSpec {
            program: "/usr/bin/false".to_owned(),
            arguments: Vec::new(),
            timeout: Duration::from_secs(1),
        })
        .await;
        assert_eq!(vetoed, StopCheckOutcome::Vetoed);

        let missing = run_stop_check(StopCheckSpec {
            program: "/path/that/does/not/exist".to_owned(),
            arguments: Vec::new(),
            timeout: Duration::from_secs(1),
        })
        .await;
        assert_eq!(missing, StopCheckOutcome::Failed);

        let timed_out = run_stop_check(StopCheckSpec {
            program: "/bin/sleep".to_owned(),
            arguments: vec!["1".to_owned()],
            timeout: Duration::from_millis(20),
        })
        .await;
        assert_eq!(timed_out, StopCheckOutcome::TimedOut);
    }
}
