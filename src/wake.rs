use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::timeout;

const LAUNCHCTL_PATH: &str = "/bin/launchctl";
const READINESS_TIMEOUT: Duration = Duration::from_millis(100);
pub(crate) const LAUNCH_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const RETRY_COOLDOWN: Duration = Duration::from_secs(5);

pub(crate) struct WakeTarget {
    upstream: std::net::SocketAddr,
    launchd_label: String,
    user_id: u32,
    state: Mutex<LaunchState>,
    launcher: Arc<dyn Launcher>,
}

#[derive(Clone, Copy, Debug)]
enum LaunchState {
    Eligible,
    Launching,
    CoolingDownUntil(Instant),
}

pub(crate) trait Launcher: Send + Sync {
    fn start(&self, target: Arc<WakeTarget>);
}

struct LaunchctlLauncher;

#[derive(Debug, Eq, PartialEq)]
enum LaunchOutcome {
    Succeeded,
    Failed,
    TimedOut,
}

#[derive(Debug, Eq, PartialEq)]
struct CommandSpec {
    program: &'static str,
    arguments: [String; 2],
    timeout: Duration,
}

impl WakeTarget {
    pub(crate) fn new(
        upstream: std::net::SocketAddr,
        launchd_label: String,
        user_id: u32,
    ) -> Arc<Self> {
        Self::with_launcher(
            upstream,
            launchd_label,
            user_id,
            Arc::new(LaunchctlLauncher),
        )
    }

    pub(crate) fn with_launcher(
        upstream: std::net::SocketAddr,
        launchd_label: String,
        user_id: u32,
        launcher: Arc<dyn Launcher>,
    ) -> Arc<Self> {
        Arc::new(Self {
            upstream,
            launchd_label,
            user_id,
            state: Mutex::new(LaunchState::Eligible),
            launcher,
        })
    }

    pub(crate) fn upstream(&self) -> std::net::SocketAddr {
        self.upstream
    }

    pub(crate) async fn is_ready(&self) -> bool {
        matches!(
            timeout(READINESS_TIMEOUT, TcpStream::connect(self.upstream)).await,
            Ok(Ok(_))
        )
    }

    pub(crate) fn request_launch(self: &Arc<Self>) {
        self.request_launch_at(Instant::now());
    }

    pub(crate) fn allow_launch_after_stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        *state = LaunchState::Eligible;
    }

    pub(crate) fn launch_in_progress(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        matches!(*state, LaunchState::Launching)
    }

    fn request_launch_at(self: &Arc<Self>, now: Instant) {
        let should_start = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            match *state {
                LaunchState::Eligible => {
                    *state = LaunchState::Launching;
                    true
                }
                LaunchState::Launching => false,
                LaunchState::CoolingDownUntil(until) if now < until => false,
                LaunchState::CoolingDownUntil(_) => {
                    *state = LaunchState::Launching;
                    true
                }
            }
        };

        if should_start {
            self.launcher.start(Arc::clone(self));
        }
    }

    fn launch_finished_at(&self, now: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        *state = LaunchState::CoolingDownUntil(now + RETRY_COOLDOWN);
    }

    fn command_spec(&self) -> CommandSpec {
        CommandSpec {
            program: LAUNCHCTL_PATH,
            arguments: [
                "kickstart".to_owned(),
                format!("gui/{}/{}", self.user_id, self.launchd_label),
            ],
            timeout: LAUNCH_TIMEOUT,
        }
    }
}

impl Launcher for LaunchctlLauncher {
    fn start(&self, target: Arc<WakeTarget>) {
        tokio::spawn(async move {
            log::info!(
                "launchctl kickstart started for configured service {}",
                target.launchd_label
            );
            let outcome = run_bounded_command(target.command_spec()).await;
            target.launch_finished_at(Instant::now());
            match outcome {
                LaunchOutcome::Succeeded => log::info!(
                    "launchctl kickstart completed for configured service {}",
                    target.launchd_label
                ),
                LaunchOutcome::Failed => log::warn!(
                    "launchctl kickstart failed for configured service {}",
                    target.launchd_label
                ),
                LaunchOutcome::TimedOut => log::warn!(
                    "launchctl kickstart timed out for configured service {}",
                    target.launchd_label
                ),
            }
        });
    }
}

async fn run_bounded_command(spec: CommandSpec) -> LaunchOutcome {
    let mut command = Command::new(spec.program);
    command
        .args(spec.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let Ok(mut child) = command.spawn() else {
        return LaunchOutcome::Failed;
    };
    match timeout(spec.timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => LaunchOutcome::Succeeded,
        Ok(_) => LaunchOutcome::Failed,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            LaunchOutcome::TimedOut
        }
    }
}

pub(crate) fn current_user_id() -> u32 {
    // SAFETY: getuid takes no arguments and has no safety preconditions.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct CountingLauncher {
        calls: AtomicUsize,
    }

    impl Launcher for CountingLauncher {
        fn start(&self, _target: Arc<WakeTarget>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn concurrent_requests_and_cooldown_deduplicate_launches() {
        let launcher = Arc::new(CountingLauncher::default());
        let target = WakeTarget::with_launcher(
            "127.0.0.1:19001".parse().unwrap(),
            "net.test.wake".to_owned(),
            501,
            launcher.clone(),
        );
        let barrier = Arc::new(Barrier::new(20));
        std::thread::scope(|scope| {
            for _ in 0..20 {
                let target = Arc::clone(&target);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    target.request_launch();
                });
            }
        });
        assert_eq!(launcher.calls.load(Ordering::SeqCst), 1);
        assert!(target.launch_in_progress());

        let finished = Instant::now();
        target.launch_finished_at(finished);
        assert!(!target.launch_in_progress());
        for offset in [
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(4),
        ] {
            target.request_launch_at(finished + offset);
        }
        assert_eq!(launcher.calls.load(Ordering::SeqCst), 1);

        target.request_launch_at(finished + RETRY_COOLDOWN);
        assert_eq!(launcher.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn launchctl_command_is_fixed_and_has_no_force_flag() {
        let target = WakeTarget::new(
            "127.0.0.1:19001".parse().unwrap(),
            "net.test.wake".to_owned(),
            502,
        );
        assert_eq!(
            target.command_spec(),
            CommandSpec {
                program: "/bin/launchctl",
                arguments: ["kickstart".to_owned(), "gui/502/net.test.wake".to_owned()],
                timeout: Duration::from_secs(5),
            }
        );
        assert!(
            !target
                .command_spec()
                .arguments
                .iter()
                .any(|value| value == "-k")
        );
    }

    #[test]
    fn intentional_stop_makes_the_next_wake_immediately_eligible() {
        let launcher = Arc::new(CountingLauncher::default());
        let target = WakeTarget::with_launcher(
            "127.0.0.1:19001".parse().unwrap(),
            "net.test.wake".to_owned(),
            501,
            launcher.clone(),
        );
        target.request_launch();
        assert_eq!(launcher.calls.load(Ordering::SeqCst), 1);
        target.launch_finished_at(Instant::now());

        target.allow_launch_after_stop();
        target.request_launch();
        assert_eq!(launcher.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn bounded_command_reports_failure_and_timeout() {
        let failed = run_bounded_command(CommandSpec {
            program: "/path/that/does/not/exist",
            arguments: [String::new(), String::new()],
            timeout: Duration::from_secs(5),
        })
        .await;
        assert_eq!(failed, LaunchOutcome::Failed);

        let timed_out = run_bounded_command(CommandSpec {
            program: "/usr/bin/yes",
            arguments: ["bounded".to_owned(), "launch".to_owned()],
            timeout: Duration::from_millis(20),
        })
        .await;
        assert_eq!(timed_out, LaunchOutcome::TimedOut);
    }

    #[tokio::test]
    async fn readiness_reflects_the_loopback_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let target = WakeTarget::new(address, "net.test.ready".to_owned(), 501);
        assert!(target.is_ready().await);
        drop(listener);
        let deadline = Instant::now() + Duration::from_secs(1);
        while target.is_ready().await && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!target.is_ready().await);
    }
}
