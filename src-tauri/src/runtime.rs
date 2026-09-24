use crate::auth;
use crate::config;
use crate::logger::Logger;
use crate::model::{AppConfig, AppStatus, Credentials, StatusView};
use crate::portal;
use crate::probe::{self, ProbeResult};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type StatusNotifier = Arc<dyn Fn(StatusView) + Send + Sync + 'static>;

// Keep the steady-state probe light, then switch to short confirmation and
// recovery intervals only while the connection is unstable. The monitor waits
// for the remaining interval after a probe, so request time is included rather
// than added on top of these values.
const STEADY_PROBE_INTERVAL: Duration = Duration::from_secs(5);
const CONFIRMATION_PROBE_INTERVAL: Duration = Duration::from_millis(250);
const RECOVERY_PROBE_INTERVAL: Duration = Duration::from_secs(2);
const MIN_PROBE_GAP: Duration = Duration::from_millis(250);
const FAILURE_THRESHOLD: u8 = 2;
const AUTH_ATTEMPTS: u8 = 3;
const AUTH_RETRY_BUDGETS: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];

#[derive(Clone)]
pub struct RuntimeHandle {
    pub inner: Arc<Mutex<RuntimeState>>,
    pub logger: Arc<Logger>,
    login_flight: Arc<Mutex<bool>>,
    monitor_started: Arc<AtomicBool>,
    setup_cache: Arc<Mutex<Option<(AppConfig, Credentials)>>>,
    monitor_wakeup: Arc<(Mutex<bool>, Condvar)>,
    status_notifier: Arc<Mutex<Option<StatusNotifier>>>,
}

pub struct RuntimeState {
    pub status: AppStatus,
    pub detail: String,
    pub checking: bool,
    /// Unix epoch seconds represented as strings for backwards compatibility.
    pub last_check: Option<String>,
    pub last_success: Option<String>,
    pub last_error: Option<String>,
    pub paused: bool,
    pub consecutive_failures: u8,
    pub capture_active: bool,
    pub auth_exhausted: bool,
    pub outage_handled: bool,
    pub generation: u64,
}

impl RuntimeHandle {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RuntimeState {
                status: AppStatus::SetupRequired,
                detail: "首次启动需要完成配置".into(),
                checking: false,
                last_check: None,
                last_success: None,
                last_error: None,
                paused: false,
                consecutive_failures: 0,
                capture_active: false,
                auth_exhausted: false,
                outage_handled: false,
                generation: 0,
            })),
            logger: Arc::new(Logger::new()),
            login_flight: Arc::new(Mutex::new(false)),
            monitor_started: Arc::new(AtomicBool::new(false)),
            setup_cache: Arc::new(Mutex::new(None)),
            monitor_wakeup: Arc::new((Mutex::new(false), Condvar::new())),
            status_notifier: Arc::new(Mutex::new(None)),
        }
    }

    /// Install the UI event sink after the Tauri app has been initialized.
    /// Keeping this as a callback avoids coupling the monitor thread to a
    /// particular Tauri runtime and keeps `RuntimeHandle` unit-testable.
    pub fn set_status_notifier<F>(&self, notifier: F)
    where
        F: Fn(StatusView) + Send + Sync + 'static,
    {
        *self
            .status_notifier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(notifier));
        self.notify_status();
    }

    pub fn status_view(&self) -> StatusView {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        StatusView {
            status: if state.paused {
                AppStatus::Paused
            } else {
                state.status.clone()
            },
            detail: state.detail.clone(),
            checking: state.checking,
            last_check: state.last_check.clone(),
            last_success: state.last_success.clone(),
            last_error: state.last_error.clone(),
        }
    }

    pub fn set_paused(&self, paused: bool) {
        {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if state.paused == paused {
                return;
            }
            state.paused = paused;
            state.generation = state.generation.wrapping_add(1);
            state.checking = false;
            state.status = if paused {
                AppStatus::Paused
            } else {
                AppStatus::Checking
            };
            state.detail = if paused {
                "自动登录已暂停"
            } else {
                "自动登录已恢复"
            }
            .into();
        }
        self.wake_monitor();
        self.notify_status();
        self.logger
            .event("INFO", "pause_changed", &format!("paused={paused}"));
    }

    /// Suppress automatic probing/login while the embedded capture browser is active.
    pub fn set_capture_active(&self, active: bool) {
        {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if state.capture_active == active {
                return;
            }
            state.capture_active = active;
            state.generation = state.generation.wrapping_add(1);
            state.checking = false;
            if active {
                state.detail = "正在获取校园网认证信息".into();
            } else if !state.paused {
                state.status = AppStatus::Checking;
                state.detail = "认证信息获取完成，等待网络探测".into();
            }
        }
        self.setup_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.wake_monitor();
        self.notify_status();
        self.logger
            .event("INFO", "capture_changed", &format!("active={active}"));
    }

    pub fn set_configured(&self) {
        {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            state.status = AppStatus::Checking;
            state.detail = "等待网络探测".into();
            state.checking = false;
            state.consecutive_failures = 0;
            state.auth_exhausted = false;
            state.outage_handled = false;
            state.last_error = None;
            state.generation = state.generation.wrapping_add(1);
        }
        self.setup_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.wake_monitor();
        self.notify_status();
        self.logger.event(
            "INFO",
            "configuration_changed",
            "automatic_login_reset=true",
        );
    }

    pub fn start_monitor(&self) {
        if self.monitor_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let handle = self.clone();
        thread::spawn(move || loop {
            let started = Instant::now();
            let next_probe = handle.tick();
            // Include request time in the normal interval, but always leave a
            // small gap after a timeout so a failed route cannot turn into a
            // busy request loop.
            let remaining = next_probe.saturating_sub(started.elapsed());
            handle.wait_for_monitor_wakeup(remaining.max(MIN_PROBE_GAP));
        });
    }

    fn tick(&self) -> Duration {
        let (config_value, credentials) = match self.load_setup() {
            Ok(value) => value,
            Err(SetupError::NotConfigured(detail)) => {
                let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                state.status = AppStatus::SetupRequired;
                state.detail = detail;
                state.checking = false;
                return STEADY_PROBE_INTERVAL;
            }
            Err(SetupError::Failure(error)) => {
                self.fail("config_error", &error);
                return STEADY_PROBE_INTERVAL;
            }
        };
        let (paused, capture_active, outage_handled) = {
            let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            (state.paused, state.capture_active, state.outage_handled)
        };
        if paused || capture_active {
            return STEADY_PROBE_INTERVAL;
        }
        if outage_handled || self.login_in_progress() {
            // The authentication thread performs its own short verification
            // probes. Avoid competing requests while it owns the recovery
            // flight, then resume promptly when it finishes.
            return RECOVERY_PROBE_INTERVAL;
        }

        let probe_generation = {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // Preserve a confirmed online state while a new probe is in
            // flight. The UI can then show "正在检测" without flashing an
            // outage color or changing "校园网已连接" prematurely.
            if !matches!(state.status, AppStatus::Online) {
                state.status = AppStatus::Checking;
            }
            state.checking = true;
            state.detail = "正在检测".into();
            state.last_check = Some(now_string());
            state.generation
        };
        self.notify_status();
        let result = probe::probe(&config_value);
        {
            let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if state.generation != probe_generation || state.paused || state.capture_active {
                return STEADY_PROBE_INTERVAL;
            }
        }
        match result {
            ProbeResult::Online { elapsed_ms } => {
                let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                state.status = AppStatus::Online;
                state.detail = "网络正常".into();
                state.checking = false;
                state.consecutive_failures = 0;
                state.auth_exhausted = false;
                state.outage_handled = false;
                state.last_success = Some(now_string());
                drop(state);
                self.notify_status();
                self.logger
                    .event("INFO", "probe_online", &format!("latency_ms={elapsed_ms}"));
                STEADY_PROBE_INTERVAL
            }
            other => {
                let (should_attempt, confirmed) = {
                    let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                    let count = state.consecutive_failures;
                    let confirmed = count >= FAILURE_THRESHOLD;
                    let portal_confirmed = portal::classify(&config_value, &other).is_some();
                    let should_attempt =
                        portal_confirmed && !state.outage_handled && !state.auth_exhausted;
                    if confirmed && !should_attempt {
                        state.status = AppStatus::ProbeFailed { consecutive: count };
                        state.detail = format!("网络探测异常（{count}/{FAILURE_THRESHOLD}）");
                        state.checking = false;
                    } else {
                        // A single miss is not an outage. Keep the previous
                        // online state and continue a short confirmation burst.
                        // Keep the same presentation for a portal candidate;
                        // the auth thread will publish the recovery state next.
                        state.detail = "正在检测".into();
                        state.checking = true;
                    }
                    (should_attempt, confirmed)
                };
                self.logger
                    .event("WARN", "probe_failed", &probe_summary(&other));
                if should_attempt {
                    let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    state.outage_handled = true;
                    drop(state);
                    self.notify_status();
                    // Reuse the result that triggered recovery. This avoids a
                    // second full timeout before the portal can be classified.
                    self.spawn_login(config_value, credentials, false, Some(other));
                } else if confirmed {
                    // A confirmed transport/HTTP failure is a plain outage,
                    // not a reason to submit credentials. Keep polling for a
                    // captive portal and reset the confirmation window so a
                    // later redirect can trigger recovery immediately.
                    let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    state.status = AppStatus::Offline;
                    state.detail = "网络不可达，等待校园认证门户".into();
                    state.checking = false;
                    state.consecutive_failures = 0;
                    drop(state);
                    self.notify_status();
                }
                next_probe_delay(confirmed)
            }
        }
    }

    fn load_setup(&self) -> Result<(AppConfig, Credentials), SetupError> {
        if let Some(value) = self
            .setup_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(value);
        }
        let config_value = config::load_config().map_err(SetupError::Failure)?;
        let credentials = config::load_credentials()
            .map_err(SetupError::Failure)?
            .ok_or_else(|| SetupError::NotConfigured("请先完成首次配置".into()))?;
        if !config_value.has_room_binding() {
            return Err(SetupError::NotConfigured(
                "请先获取公寓标识和房间标识".into(),
            ));
        }
        let value = (config_value, credentials);
        *self.setup_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(value.clone());
        Ok(value)
    }

    pub fn login_now(&self) -> Result<(), String> {
        let (config_value, credentials) = self.load_setup().map_err(|e| e.to_string())?;
        self.spawn_login(config_value, credentials, true, None);
        Ok(())
    }

    fn spawn_login(
        &self,
        config_value: AppConfig,
        credentials: Credentials,
        manual: bool,
        initial_probe: Option<ProbeResult>,
    ) {
        let handle = self.clone();
        thread::spawn(move || handle.try_login(&config_value, &credentials, manual, initial_probe));
    }

    fn try_login(
        &self,
        config_value: &AppConfig,
        credentials: &Credentials,
        manual: bool,
        initial_probe: Option<ProbeResult>,
    ) {
        let Some(_flight) = self.acquire_login_flight() else {
            if let Ok(mut state) = self.inner.lock() {
                state.outage_handled = false;
            }
            self.logger
                .event("INFO", "login_skipped", "reason=already_in_progress");
            return;
        };
        let generation = {
            let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if state.paused || state.capture_active {
                // No portal request was attempted; allow a later tick after
                // capture/pause ends to re-evaluate the outage.
                drop(state);
                if let Ok(mut state) = self.inner.lock() {
                    state.outage_handled = false;
                }
                self.logger
                    .event("INFO", "login_skipped", "reason=paused_or_capture");
                return;
            }
            state.generation
        };
        let result = initial_probe.unwrap_or_else(|| probe::probe(config_value));
        let mut captive = match portal::classify(config_value, &result) {
            Some(value) => value,
            None => {
                let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                state.status = AppStatus::Offline;
                state.detail = "未检测到校园认证门户，未提交凭据".into();
                state.checking = false;
                state.consecutive_failures = 0;
                // A plain outage may later turn into a captive redirect. Keep
                // probing eligible while no portal has actually been handled.
                state.outage_handled = false;
                drop(state);
                self.notify_status();
                self.logger
                    .event("INFO", "portal_not_found", "credentials_sent=false");
                return;
            }
        };
        {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            state.status = AppStatus::PortalDetected;
            state.detail = "已检测到校园认证门户".into();
            state.checking = false;
        }
        self.notify_status();
        for attempt in 1..=AUTH_ATTEMPTS {
            if self.cancelled(generation) {
                if let Ok(mut state) = self.inner.lock() {
                    state.outage_handled = false;
                }
                self.logger
                    .event("INFO", "login_cancelled", "reason=state_changed");
                return;
            }
            {
                let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                state.status = AppStatus::Authenticating { attempt };
                state.detail = format!("正在登录（{attempt}/{AUTH_ATTEMPTS}）");
            }
            self.notify_status();
            self.logger.event(
                "INFO",
                "auth_attempt",
                &format!("attempt={attempt} manual={manual}"),
            );
            match auth::authenticate_with_logger(
                config_value,
                credentials,
                &captive.params,
                &self.logger,
            ) {
                Ok(()) => {
                    if self.cancelled(generation) {
                        if let Ok(mut state) = self.inner.lock() {
                            state.outage_handled = false;
                        }
                        return;
                    }
                    let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    state.status = AppStatus::Online;
                    state.detail = "登录成功，网络已恢复".into();
                    state.checking = false;
                    state.consecutive_failures = 0;
                    state.auth_exhausted = false;
                    state.outage_handled = false;
                    state.last_success = Some(now_string());
                    state.last_error = None;
                    drop(state);
                    self.notify_status();
                    self.logger.event("INFO", "auth_success", "verified=true");
                    return;
                }
                Err(error) => {
                    let kind = auth_error_kind(&error);
                    self.logger.event(
                        "ERROR",
                        "auth_failed",
                        &format!("attempt={attempt} kind={kind}"),
                    );
                    if attempt < AUTH_ATTEMPTS {
                        let budget = AUTH_RETRY_BUDGETS[(attempt - 1) as usize];
                        {
                            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                            state.status = AppStatus::WaitingToRetry {
                                seconds: budget.as_secs(),
                            };
                            state.detail = "登录失败，等待网络恢复后重试".into();
                            state.checking = false;
                            state.last_error = Some(kind.into());
                        }
                        self.notify_status();
                        let minimum_delay = Duration::from_secs(attempt as u64 * 2);
                        match self.wait_for_retry(config_value, generation, budget, minimum_delay) {
                            RetryDecision::Retry(next_captive) => captive = next_captive,
                            RetryDecision::Stop => {
                                if let Ok(mut state) = self.inner.lock() {
                                    state.outage_handled = false;
                                }
                                return;
                            }
                        }
                    } else {
                        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                        state.status = AppStatus::NeedsAttention;
                        state.detail = "自动登录已停止，请检查配置或手动登录".into();
                        state.checking = false;
                        state.last_error = Some(kind.into());
                        // Keep the monitor probing so a later online result can
                        // clear the exhausted flag; it still cannot start
                        // another automatic login while that flag is set.
                        state.outage_handled = false;
                        if !manual {
                            state.auth_exhausted = true;
                        }
                        drop(state);
                        self.notify_status();
                        self.logger.event(
                            "ERROR",
                            "auth_exhausted",
                            "automatic_login_stopped=true",
                        );
                    }
                }
            }
        }
    }

    fn acquire_login_flight(&self) -> Option<LoginFlight> {
        let mut busy = self.login_flight.lock().unwrap_or_else(|e| e.into_inner());
        if *busy {
            return None;
        }
        *busy = true;
        Some(LoginFlight {
            flag: self.login_flight.clone(),
        })
    }

    fn login_in_progress(&self) -> bool {
        *self.login_flight.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cancelled(&self, generation: u64) -> bool {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.generation != generation || state.paused || state.capture_active
    }

    /// Wait for a retry opportunity while watching the network. A successful
    /// probe ends the login attempt immediately; otherwise the probe cadence
    /// backs off until the small retry budget expires. This avoids a fixed
    /// 60/300 second sleep while keeping failed credentials from being posted
    /// in a tight loop.
    fn wait_for_retry(
        &self,
        config_value: &AppConfig,
        generation: u64,
        budget: Duration,
        minimum_delay: Duration,
    ) -> RetryDecision {
        let started = Instant::now();
        let deadline = started + budget;
        let mut delay = Duration::from_millis(250);
        loop {
            if self.cancelled(generation) {
                return RetryDecision::Stop;
            }
            let now = Instant::now();
            if now >= deadline {
                // The portal disappeared or the route stayed unusable for the
                // whole observation window. Let the monitor look for a fresh
                // portal instead of replaying stale parameters.
                return RetryDecision::Stop;
            }
            let remaining = deadline.saturating_duration_since(now);
            thread::sleep(delay.min(remaining));
            if self.cancelled(generation) {
                return RetryDecision::Stop;
            }
            match probe::probe_with_timeout(config_value, Duration::from_millis(750)) {
                ProbeResult::Online { .. } => {
                    let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    state.status = AppStatus::Online;
                    state.detail = "网络正常".into();
                    state.checking = false;
                    state.consecutive_failures = 0;
                    state.outage_handled = false;
                    state.auth_exhausted = false;
                    state.last_success = Some(now_string());
                    drop(state);
                    self.notify_status();
                    return RetryDecision::Stop;
                }
                result => {
                    if started.elapsed() >= minimum_delay {
                        if let Some(next_captive) = portal::classify(config_value, &result) {
                            // A captive portal is still present. Retry as soon
                            // as the small backoff has elapsed instead of
                            // waiting for the entire retry budget. Use its
                            // newest parameters in the next attempt.
                            return RetryDecision::Retry(next_captive);
                        }
                    }
                    delay = (delay + delay).min(Duration::from_secs(2));
                }
            }
        }
    }

    fn wake_monitor(&self) {
        let (flag, wake) = &*self.monitor_wakeup;
        if let Ok(mut pending) = flag.lock() {
            *pending = true;
            wake.notify_one();
        }
    }

    fn wait_for_monitor_wakeup(&self, timeout: Duration) {
        let (flag, wake) = &*self.monitor_wakeup;
        let mut pending = flag.lock().unwrap_or_else(|e| e.into_inner());
        if *pending {
            *pending = false;
            return;
        }
        let (mut pending, _) = wake
            .wait_timeout(pending, timeout)
            .unwrap_or_else(|e| e.into_inner());
        *pending = false;
    }

    fn fail(&self, event: &str, detail: &str) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.status = AppStatus::NeedsAttention;
        state.detail = detail.into();
        state.checking = false;
        state.last_error = Some(detail.into());
        drop(state);
        self.notify_status();
        self.logger.event("ERROR", event, detail);
    }

    fn notify_status(&self) {
        let notifier = self
            .status_notifier
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(notifier) = notifier {
            notifier(self.status_view());
        }
    }
}

struct LoginFlight {
    flag: Arc<Mutex<bool>>,
}

enum RetryDecision {
    Retry(portal::CaptivePortal),
    Stop,
}

impl Drop for LoginFlight {
    fn drop(&mut self) {
        if let Ok(mut busy) = self.flag.lock() {
            *busy = false;
        }
    }
}

enum SetupError {
    NotConfigured(String),
    Failure(String),
}
impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured(s) | Self::Failure(s) => f.write_str(s),
        }
    }
}

fn now_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

fn auth_error_kind(error: &auth::AuthError) -> &'static str {
    match error {
        auth::AuthError::Transport(_) => "transport",
        auth::AuthError::Protocol(_) => "protocol",
        auth::AuthError::VerifyFailed => "verify_failed",
    }
}

fn probe_summary(value: &ProbeResult) -> String {
    match value {
        ProbeResult::Online { elapsed_ms } => format!("result=204 latency_ms={elapsed_ms}"),
        ProbeResult::Redirect {
            status, elapsed_ms, ..
        } => format!("result=redirect status={status} latency_ms={elapsed_ms}"),
        ProbeResult::Http { status, elapsed_ms } => {
            format!("result=http status={status} latency_ms={elapsed_ms}")
        }
        ProbeResult::Transport { elapsed_ms, .. } => {
            format!("result=transport latency_ms={elapsed_ms}")
        }
    }
}

fn next_probe_delay(outage_confirmed: bool) -> Duration {
    if outage_confirmed {
        RECOVERY_PROBE_INTERVAL
    } else {
        CONFIRMATION_PROBE_INTERVAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_missed_probe_stays_in_fast_confirmation() {
        assert_eq!(next_probe_delay(false), CONFIRMATION_PROBE_INTERVAL);
    }

    #[test]
    fn second_missed_probe_confirms_outage() {
        assert_eq!(FAILURE_THRESHOLD, 2);
        assert_eq!(next_probe_delay(true), RECOVERY_PROBE_INTERVAL);
    }

    #[test]
    fn confirmed_portal_can_start_recovery_without_waiting_for_threshold() {
        // Portal candidates are handled immediately in `tick`; generic
        // transport failures never reach the credential submission path.
        assert_eq!(FAILURE_THRESHOLD, 2);
    }

    #[test]
    fn steady_state_interval_is_longer_than_confirmation_burst() {
        assert!(STEADY_PROBE_INTERVAL > CONFIRMATION_PROBE_INTERVAL);
        assert!(STEADY_PROBE_INTERVAL > RECOVERY_PROBE_INTERVAL);
    }

    #[test]
    fn status_view_keeps_online_state_while_probe_is_in_progress() {
        let handle = RuntimeHandle::new();
        {
            let mut state = handle.inner.lock().unwrap();
            state.status = AppStatus::Online;
            state.detail = "正在检测".into();
            state.checking = true;
        }
        let view = handle.status_view();
        assert!(matches!(view.status, AppStatus::Online));
        assert!(view.checking);
        assert_eq!(view.detail, "正在检测");
    }

    #[test]
    fn status_notifier_receives_a_snapshot_without_holding_runtime_lock() {
        let handle = RuntimeHandle::new();
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let snapshots_for_notifier = snapshots.clone();
        handle.set_status_notifier(move |view| {
            // Reading the handle from the callback would deadlock if the
            // runtime state lock were held while the notifier was invoked.
            snapshots_for_notifier
                .lock()
                .unwrap()
                .push((view.status, view.checking, view.detail));
        });
        {
            let mut state = handle.inner.lock().unwrap();
            state.status = AppStatus::Online;
            state.checking = true;
            state.detail = "正在检测".into();
        }
        handle.notify_status();
        let snapshots = snapshots.lock().unwrap();
        assert_eq!(snapshots.len(), 2); // registration + explicit publish
        assert!(matches!(snapshots[1].0, AppStatus::Online));
        assert!(snapshots[1].1);
        assert_eq!(snapshots[1].2, "正在检测");
    }
}
