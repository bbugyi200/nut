//! Authenticated UPS actions on the least-privilege operator credential.
//!
//! The operator account (`nut-operator` in `upsd.users`) may only run
//! `beeper.*` and `test.battery.start.quick`/`test.battery.stop`; the server
//! enforces that allowlist, and this client never sends anything else. Reads
//! stay unauthenticated; every verb logs in with the secret from
//! `/etc/nut-cli/operator.secret`, enables `SET TRACKING ON`, and polls
//! `GET TRACKING` instead of assuming the command worked. A submission
//! timeout means "outcome unknown" and is never retried automatically.
//!
//! No secret ever reaches argv, logs, diffs, `--verbose`, or JSON: the
//! credential travels from the protected file straight into the `PASSWORD`
//! protocol line, and every error below names the file, never its content.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::{BatteryArgs, BeeperAction, BeeperArgs, NotifyArgs};
use crate::error::NutError;
use crate::host::{Host, ProcessRequest};
use crate::init::OPERATOR_SECRET_PATH;
use crate::journal::JournalError;
use crate::render::{HOOK_PATH, JOURNAL_TAG, OPERATOR_USER};
use crate::status::{Connection, OutputOptions};
use crate::ui::{UiContext, UiOptions, layout, theme};
use crate::upsd::{TrackingState, UpsdClient, UpsdError};

/// Instant commands this client is allowed to send. The server allowlist in
/// `upsd.users` is the enforcement point; this closed set is the client side.
pub const ALLOWED_INSTCMDS: &[&str] = &[
    "beeper.disable",
    "beeper.enable",
    "beeper.mute",
    "test.battery.start.quick",
    "test.battery.stop",
];

const BEEPER_VAR: &str = "ups.beeper.status";
const TEST_RESULT_VAR: &str = "ups.test.result";
const TRACKING_BUDGET: Duration = Duration::from_secs(10);
const TRACKING_INTERVAL: Duration = Duration::from_millis(200);
const TEST_RESULT_BUDGET: Duration = Duration::from_secs(120);
const TEST_RESULT_INTERVAL: Duration = Duration::from_secs(2);
/// A result observed sooner than this after submission may predate the test,
/// so the poll keeps watching for a change.
const MIN_RESULT_OBSERVE: Duration = Duration::from_secs(10);
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll budgets. Production uses `PollPolicy::default()`; tests pass tight
/// budgets so timeout paths stay fast.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PollPolicy {
    tracking_budget: Duration,
    tracking_interval: Duration,
    result_budget: Duration,
    result_interval: Duration,
    min_observe: Duration,
}

impl Default for PollPolicy {
    fn default() -> Self {
        Self {
            tracking_budget: TRACKING_BUDGET,
            tracking_interval: TRACKING_INTERVAL,
            result_budget: TEST_RESULT_BUDGET,
            result_interval: TEST_RESULT_INTERVAL,
            min_observe: MIN_RESULT_OBSERVE,
        }
    }
}

/// Read the operator secret without ever exposing its bytes. Errors name the
/// credential file and the group membership that unlocks it, never the secret.
fn operator_password<H: Host>(host: &H) -> Result<String, NutError> {
    let path = Path::new(OPERATOR_SECRET_PATH);
    let bytes = host.read_protected(path).map_err(|error| {
        if error.is_permission_denied() {
            NutError::operational(format!(
                "cannot read {OPERATOR_SECRET_PATH}: permission denied"
            ))
            .with_hint(
                "join the nut-operators group (sudo usermod -aG nut-operators \"$USER\"), log back in, or run with sudo",
            )
        } else {
            NutError::operational(error.to_string())
        }
    })?;
    let bytes = bytes.ok_or_else(|| {
        NutError::operational(format!(
            "operator credential is not installed at {OPERATOR_SECRET_PATH}"
        ))
        .with_hint("run `sudo nut init` to converge the operator account")
    })?;
    let password = String::from_utf8(bytes).map_err(|_| {
        NutError::operational(format!(
            "operator credential at {OPERATOR_SECRET_PATH} is not valid UTF-8"
        ))
        .with_hint("run `sudo nut init --rotate-secrets` to mint a fresh credential")
    })?;
    let password = password.trim_end_matches(['\n', '\r']).to_owned();
    if password.is_empty() || password.len() > 512 {
        return Err(NutError::operational(format!(
            "operator credential at {OPERATOR_SECRET_PATH} is missing or too long"
        ))
        .with_hint("run `sudo nut init --rotate-secrets` to mint a fresh credential"));
    }
    if password.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(NutError::operational(format!(
            "operator credential at {OPERATOR_SECRET_PATH} contains a control character"
        ))
        .with_hint("run `sudo nut init --rotate-secrets` to mint a fresh credential"));
    }
    Ok(password)
}

/// How a tracked `INSTCMD` submission ended. Only `Completed` claims the
/// command ran; every other outcome leaves the caller to report honestly.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TrackEnd {
    Completed,
    Interrupted,
    Rejected(String),
    Unknown,
}

fn map_upsd_error(error: UpsdError) -> NutError {
    NutError::operational(format!(
        "{error}; check that upsd is running and the configured UPS name is correct"
    ))
}

fn map_login_error(error: UpsdError) -> NutError {
    match &error {
        UpsdError::Server { code, .. }
            if matches!(
                code.as_str(),
                "ACCESS-DENIED" | "USERNAME-REQUIRED" | "PASSWORD-REQUIRED"
            ) =>
        {
            NutError::operational(format!(
                "upsd refused the {OPERATOR_USER} login ({code}); the credential may be stale"
            ))
            .with_hint("run `sudo nut init` to re-converge the operator account")
        }
        _ => map_upsd_error(error),
    }
}

/// An authenticated, tracking-enabled upsd session for one UPS.
struct AuthedClient {
    client: UpsdClient,
}

impl AuthedClient {
    fn connect<H: Host>(
        host: &H,
        connection: &Connection,
        password: &str,
    ) -> Result<Self, NutError> {
        let _ = host;
        let mut client = UpsdClient::connect(&connection.address, connection.port, &connection.ups)
            .map_err(map_upsd_error)?;
        client
            .login(OPERATOR_USER, password)
            .map_err(map_login_error)?;
        if !client.set_tracking(true).map_err(map_upsd_error)? {
            client.logout();
            return Err(NutError::operational(
                "upsd did not enable command tracking for the operator session",
            ));
        }
        Ok(Self { client })
    }

    fn submit(&mut self, command: &str) -> Result<String, NutError> {
        debug_assert!(ALLOWED_INSTCMDS.contains(&command));
        match self.client.instcmd(command).map_err(map_command_error)? {
            Some(id) => Ok(id),
            None => Err(NutError::operational(
                "upsd answered without a tracking id after SET TRACKING ON",
            )),
        }
    }

    fn wait_tracked<H: Host>(
        &mut self,
        host: &H,
        id: &str,
        policy: PollPolicy,
        interrupt: &AtomicBool,
    ) -> Result<TrackEnd, NutError> {
        let started = Instant::now();
        loop {
            match self.client.get_tracking(id).map_err(map_upsd_error)? {
                TrackingState::Success => return Ok(TrackEnd::Completed),
                TrackingState::Failed(reason) => return Ok(TrackEnd::Rejected(reason)),
                TrackingState::Pending => {}
            }
            if interrupt.load(Ordering::Relaxed) {
                return Ok(TrackEnd::Interrupted);
            }
            if started.elapsed() >= policy.tracking_budget {
                return Ok(TrackEnd::Unknown);
            }
            host.sleep(policy.tracking_interval);
        }
    }

    fn logout(mut self) {
        self.client.logout();
    }
}

fn map_command_error(error: UpsdError) -> NutError {
    match &error {
        UpsdError::Server { code, .. } if code == "ACCESS-DENIED" => NutError::operational(
            "upsd refused the command: the operator credential only allows beeper.* and the quick battery test",
        )
        .with_hint("power-cutting commands are never available to this credential"),
        UpsdError::Server { code, .. } if code == "CMD-NOT-SUPPORTED" => {
            NutError::operational("the driver does not support that command on this UPS")
        }
        UpsdError::Server { code, .. } if code == "UNKNOWN-UPS" => {
            NutError::operational(format!("{error}; check the configured UPS name"))
        }
        _ => map_upsd_error(error),
    }
}

fn output_error(error: std::io::Error) -> NutError {
    NutError::operational(format!("cannot write output: {error}"))
}

fn write_label<W: Write>(
    stdout: &mut W,
    label: &str,
    value: &str,
    width: usize,
) -> Result<(), NutError> {
    writeln!(stdout, "{}", layout::label_value(label, value, width)).map_err(output_error)
}

fn connect_read(connection: &Connection) -> Result<UpsdClient, NutError> {
    UpsdClient::connect(&connection.address, connection.port, &connection.ups)
        .map_err(map_upsd_error)
}

/// Resolve the loopback upsd connection shared by the action commands.
fn action_connection<H: Host>(
    host: &H,
    options: OutputOptions<'_>,
    ups: Option<&str>,
    address: Option<&str>,
    port: Option<u16>,
) -> Result<(UiContext, Connection), NutError> {
    let connection =
        crate::status::resolve_connection(host, options.config_path, ups, address, port)?;
    let ui = UiContext::new(
        UiOptions {
            color: options.color,
            ascii: options.ascii,
        },
        host,
        host.stdout_is_terminal(),
    );
    Ok((ui, connection))
}

/// Fail unless both standard streams are interactive. Agents, pipes, and cron
/// can never start or stop a battery test.
fn require_action_tty<H: Host>(host: &H, command: &str) -> Result<(), NutError> {
    if host.stdin_is_terminal() && host.stdout_is_terminal() {
        Ok(())
    } else {
        Err(NutError::operational(format!(
            "{command} needs an interactive terminal on stdin and stdout"
        )))
    }
}

/// Arm a SIGINT flag for the battery-test poll loops. The guard unregisters
/// the handler on drop so test processes never leak it.
struct InterruptGuard {
    flag: Arc<AtomicBool>,
    id: signal_hook::SigId,
}

impl InterruptGuard {
    fn arm() -> Result<Self, NutError> {
        let flag = Arc::new(AtomicBool::new(false));
        let id = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag))
            .map_err(|error| {
                NutError::operational(format!("cannot arm the interrupt handler: {error}"))
            })?;
        Ok(Self { flag, id })
    }

    fn flag(&self) -> &AtomicBool {
        &self.flag
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.id);
    }
}

fn confirm_start<H: Host>(host: &H) -> Result<(), NutError> {
    let answer = host
        .prompt_line("The UPS will briefly run on battery. Start the test? [y/N] ")
        .map_err(|error| NutError::operational(error.to_string()))?;
    if answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes") {
        Ok(())
    } else {
        Err(NutError::operational("Battery test not started."))
    }
}

#[derive(Clone, Debug, Serialize)]
struct BeeperDocument {
    schema_version: u32,
    ups: String,
    beeper_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
}

/// Show or change the UPS beeper. Bare `nut beeper` is a read; verbs
/// authenticate and confirm the new `ups.beeper.status` afterwards.
pub fn run_beeper<H: Host, W: Write>(
    args: &BeeperArgs,
    options: OutputOptions<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<(), NutError> {
    let (ui, connection) = action_connection(
        host,
        options,
        args.ups.as_deref(),
        args.upsd_address.as_deref(),
        args.upsd_port,
    )?;
    let title = format!("nut beeper · {}", connection.ups);
    match &args.action {
        None => {
            let status = connect_read(&connection)?
                .get_var(BEEPER_VAR)
                .map_err(map_upsd_error)?;
            if options.json {
                write_json(
                    stdout,
                    &BeeperDocument {
                        schema_version: 1,
                        ups: connection.ups.clone(),
                        beeper_status: status.clone(),
                        previous_status: None,
                        command: None,
                    },
                )?;
            } else {
                writeln!(
                    stdout,
                    "{}",
                    theme::Theme::paint(theme::Tone::Info, &title, ui.color_enabled)
                )
                .map_err(output_error)?;
                match status.as_deref() {
                    Some(state) => {
                        write_label(stdout, "Beeper", &describe_beeper(state), ui.width)?
                    }
                    None => {
                        write_label(stdout, "Beeper", "Not reported", ui.width)?;
                        writeln!(
                            stdout,
                            "  {} upsd does not publish {BEEPER_VAR} for this UPS.",
                            ui.glyphs.arrow
                        )
                        .map_err(output_error)?;
                    }
                }
            }
            Ok(())
        }
        Some(action) => {
            let (command, expected) = match action {
                BeeperAction::Mute => ("beeper.mute", "muted"),
                BeeperAction::Enable => ("beeper.enable", "enabled"),
                BeeperAction::Disable => ("beeper.disable", "disabled"),
            };
            let password = operator_password(host)?;
            let mut session = AuthedClient::connect(host, &connection, &password)?;
            let previous = session.client.get_var(BEEPER_VAR).map_err(map_upsd_error)?;
            let id = session.submit(command)?;
            let never = AtomicBool::new(false);
            match session.wait_tracked(host, &id, PollPolicy::default(), &never)? {
                TrackEnd::Completed => {}
                TrackEnd::Rejected(reason) => {
                    session.logout();
                    return Err(NutError::operational(format!(
                        "upsd reported the beeper command failed: {reason}"
                    )));
                }
                TrackEnd::Interrupted | TrackEnd::Unknown => {
                    session.logout();
                    return Err(NutError::operational(
                        "the beeper command outcome is unknown and was not retried",
                    )
                    .with_hint("check the beeper with `nut beeper`"));
                }
            }
            let status = session.client.get_var(BEEPER_VAR).map_err(map_upsd_error)?;
            let confirmed = status
                .as_deref()
                .is_some_and(|state| state.eq_ignore_ascii_case(expected));
            session.logout();
            if !confirmed {
                return Err(NutError::operational(format!(
                    "beeper still reports {} after {command}",
                    status.as_deref().unwrap_or("nothing")
                ))
                .with_hint("check the driver with `nut battery --all`"));
            }
            let transition = match previous.as_deref() {
                Some(before) if !before.eq_ignore_ascii_case(expected) => {
                    format!(
                        "{} (was {})",
                        describe_beeper(expected),
                        describe_beeper(before)
                    )
                }
                _ => describe_beeper(expected),
            };
            if options.json {
                write_json(
                    stdout,
                    &BeeperDocument {
                        schema_version: 1,
                        ups: connection.ups.clone(),
                        beeper_status: status.clone(),
                        previous_status: previous,
                        command: Some(command.into()),
                    },
                )?;
            } else {
                writeln!(
                    stdout,
                    "{}",
                    theme::Theme::paint(theme::Tone::Info, &title, ui.color_enabled)
                )
                .map_err(output_error)?;
                write_label(
                    stdout,
                    "Beeper",
                    &theme::Theme::paint(theme::Tone::Ok, &transition, ui.color_enabled),
                    ui.width,
                )?;
            }
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct BatteryTestDocument {
    schema_version: u32,
    ups: String,
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_result: Option<String>,
    elapsed_seconds: u64,
}

/// Run or stop a guarded battery self-test. Starting needs an interactive
/// terminal and a `[y/N]` confirmation; stopping needs the terminal but no
/// confirmation. Ctrl-C sends `test.battery.stop` before giving up.
pub fn run_battery_test<H: Host, W: Write>(
    stop: bool,
    args: &BatteryArgs,
    options: OutputOptions<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<(), NutError> {
    require_action_tty(host, "nut battery test")?;
    let (ui, connection) = action_connection(
        host,
        options,
        args.ups.as_deref(),
        args.upsd_address.as_deref(),
        args.upsd_port,
    )?;
    if !stop {
        confirm_start(host)?;
    }
    let guard = InterruptGuard::arm()?;
    run_battery_test_with_policy(BatteryTest {
        stop,
        connection: &connection,
        ui: &ui,
        options,
        host,
        stdout,
        policy: PollPolicy::default(),
        interrupt: guard.flag(),
    })
}

pub(crate) struct BatteryTest<'a, H: Host, W: Write> {
    stop: bool,
    connection: &'a Connection,
    ui: &'a UiContext,
    options: OutputOptions<'a>,
    host: &'a H,
    stdout: &'a mut W,
    policy: PollPolicy,
    interrupt: &'a AtomicBool,
}

pub(crate) fn run_battery_test_with_policy<H: Host, W: Write>(
    run: BatteryTest<'_, H, W>,
) -> Result<(), NutError> {
    let BatteryTest {
        stop,
        connection,
        ui,
        options,
        host,
        stdout,
        policy,
        interrupt,
    } = run;
    let title = format!("nut battery test · {}", connection.ups);
    let password = operator_password(host)?;
    let mut session = AuthedClient::connect(host, connection, &password)?;
    if interrupt.load(Ordering::Relaxed) {
        session.logout();
        return Err(NutError::operational(
            "interrupted before submission; nothing was sent",
        ));
    }
    let command = if stop {
        "test.battery.stop"
    } else {
        "test.battery.start.quick"
    };
    let previous = session
        .client
        .get_var(TEST_RESULT_VAR)
        .map_err(map_upsd_error)?;
    let id = session.submit(command)?;
    match session.wait_tracked(host, &id, policy, interrupt)? {
        TrackEnd::Completed => {}
        TrackEnd::Rejected(reason) => {
            session.logout();
            return Err(NutError::operational(format!(
                "upsd reported the battery test command failed: {reason}"
            )));
        }
        TrackEnd::Interrupted => {
            return stop_after_interrupt(session);
        }
        TrackEnd::Unknown => {
            session.logout();
            return Err(NutError::operational(
                "timed out waiting for upsd to accept the battery test command; the outcome is unknown and was not retried",
            )
            .with_hint("check the test state with `nut battery --all`; stop a running test with `nut battery test --stop`"));
        }
    }
    if stop {
        let result = session
            .client
            .get_var(TEST_RESULT_VAR)
            .map_err(map_upsd_error)?;
        session.logout();
        if options.json {
            write_json(
                stdout,
                &BatteryTestDocument {
                    schema_version: 1,
                    ups: connection.ups.clone(),
                    command: command.into(),
                    previous_result: previous,
                    test_result: result.clone(),
                    elapsed_seconds: 0,
                },
            )?;
        } else {
            writeln!(
                stdout,
                "{}",
                theme::Theme::paint(theme::Tone::Info, &title, ui.color_enabled)
            )
            .map_err(output_error)?;
            write_label(stdout, "Sent", "test.battery.stop", ui.width)?;
            write_label(
                stdout,
                "Battery test",
                result.as_deref().unwrap_or("Not reported"),
                ui.width,
            )?;
        }
        return Ok(());
    }
    let started = Instant::now();
    let progress = if options.json {
        Progress::Silent
    } else if host.stdout_is_terminal() {
        Progress::Spinner
    } else {
        Progress::Lines
    };
    let outcome = poll_test_result(ResultPoll {
        session: &mut session,
        host,
        previous: previous.as_deref(),
        started,
        policy,
        interrupt,
        progress,
        stdout,
    })?;
    let elapsed = started.elapsed().as_secs();
    match outcome {
        TestPoll::Done(result) => {
            session.logout();
            if options.json {
                write_json(
                    stdout,
                    &BatteryTestDocument {
                        schema_version: 1,
                        ups: connection.ups.clone(),
                        command: command.into(),
                        previous_result: previous,
                        test_result: Some(result.clone()),
                        elapsed_seconds: elapsed,
                    },
                )?;
            } else {
                if host.stdout_is_terminal() {
                    writeln!(stdout).map_err(output_error)?;
                }
                writeln!(
                    stdout,
                    "{}",
                    theme::Theme::paint(theme::Tone::Info, &title, ui.color_enabled)
                )
                .map_err(output_error)?;
                write_label(
                    stdout,
                    "Battery test",
                    &theme::Theme::paint(
                        theme::Tone::Ok,
                        &format!("Finished: {result}"),
                        ui.color_enabled,
                    ),
                    ui.width,
                )?;
            }
            Ok(())
        }
        TestPoll::Interrupted => stop_after_interrupt(session),
        TestPoll::Timeout(last) => {
            session.logout();
            Err(NutError::operational(format!(
                "battery test result is still {} after {}s; the test may still be running",
                last.as_deref().unwrap_or("not reported"),
                policy.result_budget.as_secs(),
            ))
            .with_hint("check the test state with `nut battery --all`; stop it with `nut battery test --stop`"))
        }
        TestPoll::Unreported => {
            session.logout();
            Err(NutError::operational(
                "upsd accepted the test but never reported ups.test.result for this UPS",
            )
            .with_hint("watch the test state with `nut battery --all`"))
        }
    }
}

/// Best-effort abort after Ctrl-C: fire `test.battery.stop` without waiting,
/// then report the interruption. The stop itself stays untracked-on-purpose:
/// the process is already leaving and must not block on another poll.
fn stop_after_interrupt(mut session: AuthedClient) -> Result<(), NutError> {
    let _ = session.client.instcmd("test.battery.stop");
    session.logout();
    Err(
        NutError::operational("interrupted; sent test.battery.stop and gave up waiting")
            .with_hint("confirm the test state with `nut battery --all`"),
    )
}

/// How the result poll reports progress. JSON mode stays silent so stdout
/// keeps exactly one document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Progress {
    Silent,
    Lines,
    Spinner,
}

#[derive(Debug, PartialEq)]
enum TestPoll {
    Done(String),
    Interrupted,
    Timeout(Option<String>),
    Unreported,
}

/// Poll `ups.test.result` until the test leaves its in-progress state. A
/// value only counts as final when it no longer mentions progress and either
/// differs from the pre-start reading or has been stable past `min_observe`,
/// so a first read that predates the test cannot end the poll immediately.
struct ResultPoll<'a, H: Host, W: Write> {
    session: &'a mut AuthedClient,
    host: &'a H,
    previous: Option<&'a str>,
    started: Instant,
    policy: PollPolicy,
    interrupt: &'a AtomicBool,
    progress: Progress,
    stdout: &'a mut W,
}

fn poll_test_result<H: Host, W: Write>(poll: ResultPoll<'_, H, W>) -> Result<TestPoll, NutError> {
    let ResultPoll {
        session,
        host,
        previous,
        started,
        policy,
        interrupt,
        progress,
        stdout,
    } = poll;
    let mut last: Option<String> = None;
    let mut since_output = usize::MAX;
    loop {
        if interrupt.load(Ordering::Relaxed) {
            return Ok(TestPoll::Interrupted);
        }
        let value = session
            .client
            .get_var(TEST_RESULT_VAR)
            .map_err(map_upsd_error)?;
        let elapsed = started.elapsed();
        if value != last {
            last = value.clone();
            since_output = usize::MAX;
            match progress {
                Progress::Silent => {}
                Progress::Spinner => {
                    write!(stdout, "\r  Test in progress ({elapsed:?})").ok();
                }
                Progress::Lines => {
                    if let Some(current) = value.as_deref() {
                        writeln!(stdout, "  Test in progress: {current} ({elapsed:?})").ok();
                    }
                }
            }
            let _ = stdout.flush();
        } else {
            since_output = since_output.saturating_add(1);
            match progress {
                Progress::Silent => {}
                Progress::Spinner => {
                    write!(stdout, "\r  Test in progress ({elapsed:?})").ok();
                    let _ = stdout.flush();
                }
                Progress::Lines => {
                    if since_output >= 5 {
                        since_output = 0;
                        writeln!(stdout, "  Still waiting ({elapsed:?})").ok();
                        let _ = stdout.flush();
                    }
                }
            }
        }
        if let Some(current) = last.as_deref() {
            let settled = !current.to_ascii_lowercase().contains("progress")
                && (Some(current) != previous || elapsed >= policy.min_observe);
            if settled {
                return Ok(TestPoll::Done(current.to_owned()));
            }
        } else if elapsed >= policy.min_observe {
            return Ok(TestPoll::Unreported);
        }
        if elapsed >= policy.result_budget {
            return Ok(TestPoll::Timeout(last));
        }
        host.sleep(policy.result_interval);
    }
}

#[derive(Clone, Debug, Serialize)]
struct NotifyEvent {
    timestamp_us: u64,
    message: String,
}

#[derive(Clone, Debug, Serialize)]
struct NotifyDocument {
    schema_version: u32,
    from: String,
    to: String,
    access: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    events: Vec<NotifyEvent>,
}

#[derive(Clone, Debug, Serialize)]
struct NotifyTestDocument {
    schema_version: u32,
    hook: String,
    recipient: String,
    exit_code: i32,
}

/// Show notification recipients and recent notification events, or run the
/// installed hook's `test-notify` event. The test path needs no privilege and
/// agents may run it; delivery itself must be confirmed in the inbox.
pub fn run_notify<H: Host, W: Write>(
    args: &NotifyArgs,
    options: OutputOptions<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<(), NutError> {
    match &args.action {
        None => run_notify_show(options, host, stdout),
        Some(crate::cli::NotifyAction::Test) => run_notify_test(options, host, stdout),
    }
}

fn run_notify_show<H: Host, W: Write>(
    options: OutputOptions<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<(), NutError> {
    let (_, _, config) = crate::config::load_config(host, options.config_path)
        .map_err(|error| NutError::operational(error.to_string()))?;
    let ui = UiContext::new(
        UiOptions {
            color: options.color,
            ascii: options.ascii,
        },
        host,
        host.stdout_is_terminal(),
    );
    let title = format!("nut notify · {}", config.host.name);
    let events_args = crate::cli::EventsArgs {
        limit: Some(50),
        ..Default::default()
    };
    let history = match crate::journal::read(host, &events_args, options.verbose) {
        Ok(history) => Some(history),
        Err(JournalError::Access) => None,
        Err(JournalError::Read(message)) => {
            return Err(NutError::operational(message).with_hint("journalctl --system --no-pager"));
        }
    };
    // Journal entries arrive oldest first; keep the five most recent hook
    // events and show them newest first.
    let mut events: Vec<NotifyEvent> = history
        .as_ref()
        .map(|history| {
            history
                .entries
                .iter()
                .filter(|entry| entry.source == JOURNAL_TAG)
                .map(|entry| NotifyEvent {
                    timestamp_us: entry.timestamp_us,
                    message: entry.message.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    let skip = events.len().saturating_sub(5);
    events = events.split_off(skip);
    events.reverse();
    if options.json {
        write_json(
            stdout,
            &match history {
                Some(_) => NotifyDocument {
                    schema_version: 1,
                    from: config.notify.from.clone(),
                    to: config.notify.to.clone(),
                    access: "ok".into(),
                    message: None,
                    events,
                },
                None => NotifyDocument {
                    schema_version: 1,
                    from: config.notify.from.clone(),
                    to: config.notify.to.clone(),
                    access: "denied".into(),
                    message: Some(
                        "System journal access needs root here. Run sudo nut notify.".into(),
                    ),
                    events,
                },
            },
        )?;
        if history.is_none() {
            return Err(NutError::operational(
                "System journal access needs root here. Run sudo nut notify.",
            ));
        }
        return Ok(());
    }
    writeln!(
        stdout,
        "{}",
        theme::Theme::paint(theme::Tone::Info, &title, ui.color_enabled)
    )
    .map_err(output_error)?;
    write_label(stdout, "From", &config.notify.from, ui.width)?;
    write_label(stdout, "To", &config.notify.to, ui.width)?;
    if events.is_empty() {
        match history {
            Some(_) => {
                writeln!(
                    stdout,
                    "  {} no hook notifications recorded yet.",
                    ui.glyphs.arrow
                )
                .map_err(output_error)?;
            }
            None => {
                writeln!(
                    stdout,
                    "  {} System journal access needs root here. Run sudo nut notify.",
                    ui.glyphs.warn
                )
                .map_err(output_error)?;
                return Err(NutError::operational(
                    "System journal access needs root here. Run sudo nut notify.",
                ));
            }
        }
    } else {
        writeln!(stdout, "  Recent notifications").map_err(output_error)?;
        let now_us = host
            .now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros()
            .min(u64::MAX as u128) as u64;
        for event in &events {
            writeln!(
                stdout,
                "    {} · {}",
                crate::events::time_label(event.timestamp_us, now_us, options.verbose),
                event.message
            )
            .map_err(output_error)?;
        }
    }
    Ok(())
}

fn run_notify_test<H: Host, W: Write>(
    options: OutputOptions<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<(), NutError> {
    let (_, _, config) = crate::config::load_config(host, options.config_path)
        .map_err(|error| NutError::operational(error.to_string()))?;
    let ui = UiContext::new(
        UiOptions {
            color: options.color,
            ascii: options.ascii,
        },
        host,
        host.stdout_is_terminal(),
    );
    let hook = Path::new(HOOK_PATH);
    let present = host
        .metadata(hook)
        .map_err(|error| NutError::operational(error.to_string()))?;
    if present.is_none_or(|metadata| !metadata.executable()) {
        return Err(NutError::operational(format!(
            "notification hook is not installed at {HOOK_PATH}"
        ))
        .with_hint("run `sudo nut init` to install the event hook"));
    }
    let hook_program = std::ffi::OsStr::new(HOOK_PATH);
    let event = std::ffi::OsString::from("test-notify");
    let output = host
        .run_bounded(&ProcessRequest {
            program: hook_program,
            args: std::slice::from_ref(&event),
            timeout: HOOK_TIMEOUT,
            secret: false,
            inherit_tty: false,
        })
        .map_err(|error| NutError::operational(error.to_string()))?;
    let code = output.status.unwrap_or(-1);
    if code != 0 {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        return Err(NutError::operational(if detail.is_empty() {
            format!("notification hook exited {code}")
        } else {
            format!("notification hook exited {code}: {detail}")
        }));
    }
    if options.json {
        write_json(
            stdout,
            &NotifyTestDocument {
                schema_version: 1,
                hook: HOOK_PATH.into(),
                recipient: config.notify.to.clone(),
                exit_code: code,
            },
        )?;
    } else {
        let title = format!("nut notify test · {}", config.host.name);
        writeln!(
            stdout,
            "{}",
            theme::Theme::paint(theme::Tone::Info, &title, ui.color_enabled)
        )
        .map_err(output_error)?;
        write_label(stdout, "Hook", HOOK_PATH, ui.width)?;
        writeln!(
            stdout,
            "  {} test notification sent. Confirm delivery in the inbox for {}.",
            ui.glyphs.ok, config.notify.to
        )
        .map_err(output_error)?;
    }
    Ok(())
}

fn describe_beeper(state: &str) -> String {
    match state.to_ascii_lowercase().as_str() {
        "enabled" => "Enabled".to_owned(),
        "disabled" => "Disabled".to_owned(),
        "muted" => "Muted until the next event".to_owned(),
        other => format!("Reported {other:?}; expected enabled, disabled, or muted"),
    }
}

fn write_json<W: Write, T: Serialize>(stdout: &mut W, document: &T) -> Result<(), NutError> {
    serde_json::to_writer_pretty(&mut *stdout, document)
        .map_err(|error| NutError::operational(format!("cannot write JSON output: {error}")))?;
    writeln!(stdout).map_err(output_error)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_upsd::{FakeUpsdFixture, FakeUser, serve_client};
    use crate::host::FakeHost;
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::thread;

    const TEST_CONFIG: &str = "schema = 1\n\n[host]\nname = \"demo\"\n\n[ups.demo]\nprofile = \"dummy\"\n\n[shutdown]\nnotify_after = \"30s\"\nearly_after = \"180s\"\npoweroff_wait = \"30m\"\nshutdown_budget = \"2m\"\n\n[notify]\nfrom = \"root@example.invalid\"\nto = \"operator@example.invalid\"\ncommbad_after = \"60s\"\n";

    fn action_fixture() -> FakeUpsdFixture {
        FakeUpsdFixture {
            ups_name: "demo".into(),
            description: "Action UPS".into(),
            variables: BTreeMap::from([
                ("ups.status".into(), "OL CHRG".into()),
                ("ups.beeper.status".into(), "enabled".into()),
                ("ups.test.result".into(), "OK".into()),
            ]),
            commands: vec![
                "beeper.mute".into(),
                "test.battery.start.quick".into(),
                "test.battery.stop".into(),
            ],
            users: BTreeMap::from([(
                OPERATOR_USER.into(),
                FakeUser {
                    password: "action-secret".into(),
                    instcmds: ALLOWED_INSTCMDS.iter().map(ToString::to_string).collect(),
                },
            )]),
            test_result_sequence: vec!["Test in progress".into(), "OK".into()],
            ..Default::default()
        }
    }

    fn serve(mut fixture: FakeUpsdFixture) -> (u16, thread::JoinHandle<std::io::Result<()>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(stream, &mut fixture)
        });
        (port, server)
    }

    fn action_host() -> FakeHost {
        FakeHost::default()
            .with_file("/test/host.toml", TEST_CONFIG.as_bytes())
            .with_file(OPERATOR_SECRET_PATH, b"action-secret\n")
            .with_env("LANG", "C.UTF-8")
    }

    /// Build argv with endpoint overrides right after the top-level command:
    /// parent-level options must precede any nested action for clap to parse.
    fn action_args(
        port: u16,
        globals: &[&str],
        command: &str,
        rest: &[&str],
    ) -> Vec<std::ffi::OsString> {
        let mut args = vec![std::ffi::OsString::from("nut")];
        args.extend(globals.iter().map(std::ffi::OsString::from));
        args.push(std::ffi::OsString::from("--config"));
        args.push(std::ffi::OsString::from("/test/host.toml"));
        args.push(std::ffi::OsString::from(command));
        args.push(std::ffi::OsString::from("--upsd-port"));
        args.push(std::ffi::OsString::from(port.to_string()));
        args.extend(rest.iter().map(std::ffi::OsString::from));
        args
    }

    #[test]
    fn beeper_bare_shows_state_and_json() {
        let (port, server) = serve(action_fixture());
        let host = action_host();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            action_args(port, &[], "beeper", &[]),
            &host,
            &mut stdout,
            &mut stderr,
        );
        server.join().unwrap().unwrap();
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("nut beeper · demo"), "{text}");
        assert!(text.contains("Enabled"), "{text}");

        let (port, server) = serve(action_fixture());
        let host = action_host();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            action_args(port, &["--json"], "beeper", &[]),
            &host,
            &mut stdout,
            &mut stderr,
        );
        server.join().unwrap().unwrap();
        assert_eq!(code, 0);
        let document: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(document["schema_version"], 1);
        assert_eq!(document["beeper_status"], "enabled");
        assert!(document.get("command").is_none());
    }

    #[test]
    fn beeper_bare_reports_a_missing_variable() {
        let mut fixture = action_fixture();
        fixture.variables.remove(BEEPER_VAR);
        let (port, server) = serve(fixture);
        let host = action_host();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            action_args(port, &[], "beeper", &[]),
            &host,
            &mut stdout,
            &mut stderr,
        );
        server.join().unwrap().unwrap();
        assert_eq!(code, 0);
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("Not reported"), "{text}");
    }

    #[test]
    fn beeper_mute_confirms_the_new_state() {
        let (port, server) = serve(action_fixture());
        let host = action_host();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            action_args(port, &[], "beeper", &["mute"]),
            &host,
            &mut stdout,
            &mut stderr,
        );
        server.join().unwrap().unwrap();
        // The fake is scripted, so the beeper stays enabled and the command
        // must report the mismatch instead of claiming success.
        assert_eq!(code, 1);
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.is_empty(), "{text}");
        let errors = String::from_utf8(stderr).unwrap();
        assert!(errors.contains("beeper still reports"), "{errors}");
        assert!(!errors.contains("action-secret"), "{errors}");
    }

    #[test]
    fn beeper_without_a_credential_names_the_file_not_the_secret() {
        // No credential means the client errors before connecting, so the
        // server thread is detached instead of joined.
        let (port, _server) = serve(action_fixture());
        let host = FakeHost::default()
            .with_file("/test/host.toml", TEST_CONFIG.as_bytes())
            .with_env("LANG", "C.UTF-8");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            action_args(port, &[], "beeper", &["mute"]),
            &host,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 1);
        let errors = String::from_utf8(stderr).unwrap();
        assert!(errors.contains(OPERATOR_SECRET_PATH), "{errors}");
        assert!(errors.contains("nut init"), "{errors}");
    }

    #[test]
    fn beeper_refuses_an_unsupported_command() {
        let mut fixture = action_fixture();
        fixture.commands.retain(|command| command != "beeper.mute");
        let (port, server) = serve(fixture);
        let host = action_host();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            action_args(port, &[], "beeper", &["mute"]),
            &host,
            &mut stdout,
            &mut stderr,
        );
        server.join().unwrap().unwrap();
        assert_eq!(code, 1);
        let errors = String::from_utf8(stderr).unwrap();
        assert!(errors.contains("does not support"), "{errors}");
    }

    fn tty_host(prompts: &[&str]) -> FakeHost {
        action_host()
            .with_stdin_tty(true)
            .with_tty(true, true)
            .with_prompt_lines(prompts.iter().map(ToString::to_string))
    }

    #[test]
    fn battery_test_starts_and_finishes_with_json() {
        // The scripted final result differs from the pre-start reading so the
        // poll settles without waiting out the production observe window.
        let mut fixture = action_fixture();
        fixture.test_result_sequence = vec!["Test in progress".into(), "Passed".into()];
        let (port, server) = serve(fixture);
        let host = tty_host(&["y"]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            action_args(port, &[], "battery", &["test"]),
            &host,
            &mut stdout,
            &mut stderr,
        );
        server.join().unwrap().unwrap();
        assert_eq!(code, 0);
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("Finished: Passed"), "{text}");

        let mut fixture = action_fixture();
        fixture.test_result_sequence = vec!["Test in progress".into(), "Passed".into()];
        let (port, server) = serve(fixture);
        let host = tty_host(&["yes"]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            action_args(port, &["--json"], "battery", &["test"]),
            &host,
            &mut stdout,
            &mut stderr,
        );
        server.join().unwrap().unwrap();
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        let document: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(document["schema_version"], 1);
        assert_eq!(document["command"], "test.battery.start.quick");
        assert_eq!(document["previous_result"], "OK");
        assert_eq!(document["test_result"], "Passed");
    }

    #[test]
    fn battery_test_stop_reports_the_aborted_result() {
        let (port, server) = serve(action_fixture());
        let host = tty_host(&[]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            action_args(port, &[], "battery", &["test", "--stop"]),
            &host,
            &mut stdout,
            &mut stderr,
        );
        server.join().unwrap().unwrap();
        assert_eq!(code, 0);
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("test.battery.stop"), "{text}");
        assert!(text.contains("Aborted"), "{text}");
    }

    #[test]
    fn battery_test_refuses_without_a_terminal() {
        // The client errors before connecting, so the server is detached.
        let (port, _server) = serve(action_fixture());
        let host = action_host();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            action_args(port, &[], "battery", &["test"]),
            &host,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 1);
        let errors = String::from_utf8(stderr).unwrap();
        assert!(errors.contains("interactive terminal"), "{errors}");
    }

    #[test]
    fn battery_test_decline_starts_nothing() {
        // The decline precedes the connection, so the server is detached.
        let (port, _server) = serve(action_fixture());
        let host = tty_host(&["n"]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            action_args(port, &[], "battery", &["test"]),
            &host,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 1);
        let errors = String::from_utf8(stderr).unwrap();
        assert!(errors.contains("not started"), "{errors}");
    }

    fn tight_policy() -> PollPolicy {
        PollPolicy {
            tracking_budget: Duration::from_millis(200),
            tracking_interval: Duration::from_millis(5),
            result_budget: Duration::from_secs(5),
            result_interval: Duration::from_millis(5),
            min_observe: Duration::from_millis(20),
        }
    }

    fn test_connection(host: &FakeHost, port: u16) -> (UiContext, Connection) {
        let options = crate::status::OutputOptions {
            config_path: Some(Path::new("/test/host.toml")),
            json: false,
            verbose: false,
            color: crate::cli::ColorChoice::Never,
            ascii: true,
        };
        let (ui, connection) =
            action_connection(host, options, None, None, Some(port)).expect("connection");
        (ui, connection)
    }

    #[test]
    fn battery_test_timeout_is_unknown_and_unretried() {
        let mut fixture = action_fixture();
        fixture.sticky_pending = true;
        let (port, server) = serve(fixture);
        let host = tty_host(&[]);
        let (ui, connection) = test_connection(&host, port);
        let options = crate::status::OutputOptions {
            config_path: Some(Path::new("/test/host.toml")),
            json: false,
            verbose: false,
            color: crate::cli::ColorChoice::Never,
            ascii: true,
        };
        let idle = AtomicBool::new(false);
        let mut stdout = Vec::new();
        let error = run_battery_test_with_policy(BatteryTest {
            stop: false,
            connection: &connection,
            ui: &ui,
            options,
            host: &host,
            stdout: &mut stdout,
            policy: tight_policy(),
            interrupt: &idle,
        })
        .unwrap_err();
        server.join().unwrap().unwrap();
        assert!(
            error.to_string().contains("unknown and was not retried"),
            "{error:?}"
        );
    }

    /// Accept loop for tests that need a second connection after the first
    /// closes. Detached: the thread ends with the test process.
    fn serve_loop(mut fixture: FakeUpsdFixture) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        if serve_client(stream, &mut fixture).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        port
    }

    #[test]
    fn battery_test_interrupt_sends_stop() {
        let mut fixture = action_fixture();
        // Never leave the in-progress state on its own: the only way out is
        // the interrupt firing the stop command.
        fixture.test_result_sequence = vec!["Test in progress".into()];
        let port = serve_loop(fixture);
        let host = tty_host(&[]);
        let (ui, connection) = test_connection(&host, port);
        let options = crate::status::OutputOptions {
            config_path: Some(Path::new("/test/host.toml")),
            json: false,
            verbose: false,
            color: crate::cli::ColorChoice::Never,
            ascii: true,
        };
        let interrupt = AtomicBool::new(false);
        let mut stdout = Vec::new();
        let error = thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(500));
                interrupt.store(true, Ordering::Relaxed);
            });
            run_battery_test_with_policy(BatteryTest {
                stop: false,
                connection: &connection,
                ui: &ui,
                options,
                host: &host,
                stdout: &mut stdout,
                policy: tight_policy(),
                interrupt: &interrupt,
            })
            .unwrap_err()
        });
        assert!(
            error.to_string().contains("sent test.battery.stop"),
            "{error:?}"
        );
        // The stop landed: a fresh read sees the aborted result.
        let mut client =
            crate::upsd::UpsdClient::connect("127.0.0.1", port, "demo").expect("reconnect");
        assert_eq!(
            client.get_var("ups.test.result").unwrap(),
            Some("Aborted".into())
        );
    }

    const NOTIFY_JOURNAL: &str = "{\"__REALTIME_TIMESTAMP\": \"1700000000000000\", \"__CURSOR\": \"n-0\", \"_BOOT_ID\": \"boot-a\", \"SYSLOG_IDENTIFIER\": \"upsmon\", \"MESSAGE\": \"UPS demo@localhost on battery\"}\n{\"__REALTIME_TIMESTAMP\": \"1700000060000000\", \"__CURSOR\": \"n-1\", \"_BOOT_ID\": \"boot-a\", \"SYSLOG_IDENTIFIER\": \"nut-upssched\", \"MESSAGE\": \"onbatt: on battery\"}\n{\"__REALTIME_TIMESTAMP\": \"1700000120000000\", \"__CURSOR\": \"n-2\", \"_BOOT_ID\": \"boot-a\", \"SYSLOG_IDENTIFIER\": \"nut-upssched\", \"MESSAGE\": \"online: power restored\"}\n";

    fn notify_host(journal_stdout: Option<&[u8]>) -> FakeHost {
        let mut host = FakeHost::default()
            .with_file("/test/host.toml", TEST_CONFIG.as_bytes())
            .with_env("LANG", "C.UTF-8");
        if let Some(output) = journal_stdout {
            host = host.with_program(
                "journalctl",
                crate::host::ProcessOutput {
                    status: Some(0),
                    stdout: output.to_vec(),
                    stderr: Vec::new(),
                },
            );
        }
        host
    }

    #[test]
    fn notify_show_lists_recipients_and_recent_events() {
        let host = notify_host(Some(NOTIFY_JOURNAL.as_bytes()));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            vec![
                std::ffi::OsString::from("nut"),
                std::ffi::OsString::from("--config"),
                std::ffi::OsString::from("/test/host.toml"),
                std::ffi::OsString::from("notify"),
            ],
            &host,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 0);
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("root@example.invalid"), "{text}");
        assert!(text.contains("operator@example.invalid"), "{text}");
        assert!(text.contains("online: power restored"), "{text}");
        assert!(text.contains("onbatt: on battery"), "{text}");
        assert!(!text.contains("UPS demo@localhost on battery"), "{text}");
    }

    #[test]
    fn notify_show_json_reports_denied_access_with_recipients() {
        // An empty probe means journalctl saw nothing and the caller is not
        // root, so access is denied but recipients still print.
        let host = notify_host(Some(b""));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            vec![
                std::ffi::OsString::from("nut"),
                std::ffi::OsString::from("--config"),
                std::ffi::OsString::from("/test/host.toml"),
                std::ffi::OsString::from("--json"),
                std::ffi::OsString::from("notify"),
            ],
            &host,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 1);
        let document: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(document["access"], "denied");
        assert_eq!(document["from"], "root@example.invalid");
        assert_eq!(document["to"], "operator@example.invalid");
    }

    #[test]
    fn notify_test_runs_the_hook_and_confirms_the_inbox() {
        let host = notify_host(None)
            .with_metadata(HOOK_PATH, crate::host::FileMetadata::file(0, 0, 0o755));
        let host = host.with_program(
            HOOK_PATH,
            crate::host::ProcessOutput {
                status: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            vec![
                std::ffi::OsString::from("nut"),
                std::ffi::OsString::from("--config"),
                std::ffi::OsString::from("/test/host.toml"),
                std::ffi::OsString::from("notify"),
                std::ffi::OsString::from("test"),
            ],
            &host,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 0);
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("Confirm delivery in the inbox"), "{text}");
        assert!(
            host.effects().iter().any(|effect| matches!(
                effect,
                crate::host::HostEffect::Run { program, args }
                if program == HOOK_PATH && args == &vec![std::ffi::OsString::from("test-notify")]
            )),
            "{:?}",
            host.effects()
        );
    }

    #[test]
    fn notify_test_without_a_hook_points_at_init() {
        let host = notify_host(None);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(
            vec![
                std::ffi::OsString::from("nut"),
                std::ffi::OsString::from("--config"),
                std::ffi::OsString::from("/test/host.toml"),
                std::ffi::OsString::from("notify"),
                std::ffi::OsString::from("test"),
            ],
            &host,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 1);
        let errors = String::from_utf8(stderr).unwrap();
        assert!(errors.contains("sudo nut init"), "{errors}");
    }
}
