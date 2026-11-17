use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

const MAX_COMMAND_BYTES: usize = 1024;
const MAX_VARIABLES: usize = 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeUser {
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub instcmds: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeUpsdFixture {
    pub ups_name: String,
    pub description: String,
    pub variables: BTreeMap<String, String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub clients: Vec<String>,
    /// Authenticated users. Empty means this fake predates authentication and
    /// rejects USERNAME like the old read-only fake did.
    #[serde(default)]
    pub users: BTreeMap<String, FakeUser>,
    /// Scripted `ups.test.result` values after `test.battery.start.quick`.
    /// Each value surfaces on the next `GET VAR ups.test.result` read,
    /// starting with the first value on the first read after the command.
    #[serde(default)]
    pub test_result_sequence: Vec<String>,
    /// Fake-only fault: never complete a tracked command, so clients exercise
    /// their submission-timeout path.
    #[serde(default)]
    pub sticky_pending: bool,
    /// Scripted `ups.test.result` values still to surface on later `GET VAR`
    /// reads. Runtime state only; never deserialized.
    #[serde(skip)]
    pub pending_results: VecDeque<String>,
}

impl FakeUpsdFixture {
    pub fn read(path: &Path) -> Result<Self, String> {
        let contents = fs::read_to_string(path)
            .map_err(|error| format!("cannot read fixture {}: {error}", path.display()))?;
        let fixture: Self = serde_json::from_str(&contents)
            .map_err(|error| format!("invalid fake upsd fixture: {error}"))?;
        fixture.validate()?;
        Ok(fixture)
    }

    fn validate(&self) -> Result<(), String> {
        if !valid_token(&self.ups_name, 64) {
            return Err("fixture has an invalid UPS name".into());
        }
        if self.variables.len() > MAX_VARIABLES {
            return Err(format!("fixture exceeds {MAX_VARIABLES} variables"));
        }
        validate_text(&self.description)?;
        for (name, value) in &self.variables {
            if !valid_var_name(name) {
                return Err(format!("fixture has an invalid variable name: {name:?}"));
            }
            validate_text(value)?;
            if value.len() > MAX_COMMAND_BYTES {
                return Err(format!("fixture value for {name} is too long"));
            }
        }
        for (name, user) in &self.users {
            if !valid_token(name, 64) {
                return Err(format!("fixture has an invalid username: {name:?}"));
            }
            if user.password.is_empty()
                || user.password.len() > 512
                || user
                    .password
                    .bytes()
                    .any(|byte| byte < 0x20 || byte == 0x7f)
            {
                return Err(format!("fixture user {name:?} has an invalid password"));
            }
            for command in &user.instcmds {
                if !valid_token(command, 64) {
                    return Err(format!(
                        "fixture user {name:?} allows an invalid command: {command:?}"
                    ));
                }
            }
        }
        for command in &self.commands {
            if !valid_token(command, 64) {
                return Err(format!("fixture has an invalid command: {command:?}"));
            }
        }
        for value in &self.test_result_sequence {
            validate_text(value)?;
        }
        Ok(())
    }
}

/// Run a read-only scripted upsd that accepts connections only from loopback clients.
pub fn run_fake_upsd(listen: &str, fixture_path: &Path) -> Result<(), String> {
    let mut fixture = FakeUpsdFixture::read(fixture_path)?;
    let address: SocketAddr = listen
        .parse()
        .map_err(|error| format!("invalid listen address {listen:?}: {error}"))?;
    if !address.ip().is_loopback() {
        return Err("fake upsd must bind to a loopback address".into());
    }
    let listener = TcpListener::bind(address)
        .map_err(|error| format!("cannot listen on {address}: {error}"))?;
    eprintln!(
        "fake upsd listening on {} for UPS {}",
        listener.local_addr().unwrap_or(address),
        fixture.ups_name
    );
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                if let Err(error) = serve_client(stream, &mut fixture) {
                    eprintln!("fake upsd client ended: {error}");
                }
            }
            Err(error) => return Err(format!("cannot accept fake upsd client: {error}")),
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct SessionState {
    username: Option<String>,
    password_ok: bool,
    tracking: bool,
    issued: Vec<String>,
    next_tracking: u64,
}

impl SessionState {
    fn auth_error(&self) -> Option<&'static str> {
        if self.username.is_none() {
            Some("ERR USERNAME-REQUIRED")
        } else if !self.password_ok {
            Some("ERR PASSWORD-REQUIRED")
        } else {
            None
        }
    }
}

/// Serve one fake upsd connection. Exposed for the in-process scripted client test.
pub fn serve_client(mut stream: TcpStream, fixture: &mut FakeUpsdFixture) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    writeln!(stream, "Network UPS Tools upsd 2.8.1 - scripted fake")?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut session = SessionState::default();
    loop {
        let Some(command) = read_command(&mut reader)? else {
            return Ok(());
        };
        let Some(fields) = split_command(&command) else {
            writeln!(stream, "ERR INVALID-ARGUMENT")?;
            stream.flush()?;
            continue;
        };
        let words = fields.iter().map(String::as_str).collect::<Vec<_>>();
        match words.as_slice() {
            ["VER"] => writeln!(stream, "upsd 2.8.1 - scripted fake")?,
            ["LOGOUT"] => {
                writeln!(stream, "OK Goodbye")?;
                stream.flush()?;
                return Ok(());
            }
            ["LIST", "UPS"] => {
                writeln!(stream, "BEGIN LIST UPS")?;
                writeln!(
                    stream,
                    "UPS {} \"{}\"",
                    fixture.ups_name,
                    quote_value(&fixture.description)
                )?;
                writeln!(stream, "END LIST UPS")?;
            }
            ["LIST", "VAR", ups] if ups.eq_ignore_ascii_case(&fixture.ups_name) => {
                writeln!(stream, "BEGIN LIST VAR {}", fixture.ups_name)?;
                for (name, value) in &fixture.variables {
                    writeln!(
                        stream,
                        "VAR {} {name} \"{}\"",
                        fixture.ups_name,
                        quote_value(value)
                    )?;
                }
                writeln!(stream, "END LIST VAR {}", fixture.ups_name)?;
            }
            ["GET", "VAR", ups, name] if ups.eq_ignore_ascii_case(&fixture.ups_name) => {
                let key = name.to_ascii_lowercase();
                if key == "ups.test.result"
                    && let Some(next) = fixture.pending_results.pop_front()
                {
                    fixture.variables.insert(key.clone(), next);
                }
                if let Some(value) = fixture.variables.get(&key) {
                    writeln!(
                        stream,
                        "VAR {} {key} \"{}\"",
                        fixture.ups_name,
                        quote_value(value)
                    )?;
                } else {
                    // Match real upsd 2.8.1: unsupported variables report
                    // VAR-NOT-SUPPORTED (dummy-ups has no beeper variable).
                    writeln!(stream, "ERR VAR-NOT-SUPPORTED {name}")?;
                }
            }
            ["LIST", "CMD", ups] if ups.eq_ignore_ascii_case(&fixture.ups_name) => {
                writeln!(stream, "BEGIN LIST CMD {}", fixture.ups_name)?;
                for command in &fixture.commands {
                    writeln!(stream, "CMD {} {command}", fixture.ups_name)?;
                }
                writeln!(stream, "END LIST CMD {}", fixture.ups_name)?;
            }
            ["LIST", "CLIENT", ups] if ups.eq_ignore_ascii_case(&fixture.ups_name) => {
                writeln!(stream, "BEGIN LIST CLIENT {}", fixture.ups_name)?;
                for client in &fixture.clients {
                    writeln!(stream, "CLIENT {} {client}", fixture.ups_name)?;
                }
                writeln!(stream, "END LIST CLIENT {}", fixture.ups_name)?;
            }
            ["LIST", "VAR", _]
            | ["GET", "VAR", _, _]
            | ["LIST", "CMD", _]
            | ["LIST", "CLIENT", _] => {
                writeln!(stream, "ERR UNKNOWN-UPS")?;
            }
            ["USERNAME", user] => {
                if fixture.users.is_empty() {
                    // Fixtures that predate authentication keep the old behavior.
                    writeln!(stream, "ERR UNKNOWN-COMMAND")?;
                } else if session.username.is_some() {
                    writeln!(stream, "ERR ALREADY-SET-USERNAME")?;
                } else if fixture.users.contains_key(*user) {
                    session.username = Some((*user).to_owned());
                    writeln!(stream, "OK")?;
                } else {
                    writeln!(stream, "ERR ACCESS-DENIED")?;
                }
            }
            ["PASSWORD", password] => {
                if fixture.users.is_empty() {
                    writeln!(stream, "ERR UNKNOWN-COMMAND")?;
                } else if session.username.is_none() {
                    writeln!(stream, "ERR USERNAME-REQUIRED")?;
                } else if session.password_ok {
                    writeln!(stream, "ERR ALREADY-SET-PASSWORD")?;
                } else {
                    // Unknown usernames never reach this branch: USERNAME
                    // already rejected them with ACCESS-DENIED.
                    let name = session.username.clone().unwrap_or_default();
                    if fixture
                        .users
                        .get(&name)
                        .is_some_and(|user| user.password == *password)
                    {
                        session.password_ok = true;
                        writeln!(stream, "OK")?;
                    } else {
                        writeln!(stream, "ERR ACCESS-DENIED")?;
                    }
                }
            }
            ["SET", "TRACKING", value] => {
                if fixture.users.is_empty() {
                    writeln!(stream, "ERR UNKNOWN-COMMAND")?;
                } else if let Some(error) = session.auth_error() {
                    writeln!(stream, "{error}")?;
                } else if !matches!(*value, "ON" | "OFF") {
                    writeln!(stream, "ERR INVALID-ARGUMENT")?;
                } else {
                    session.tracking = *value == "ON";
                    writeln!(stream, "OK")?;
                }
            }
            ["GET", "TRACKING"] => {
                writeln!(stream, "{}", if session.tracking { "ON" } else { "OFF" })?;
            }
            ["GET", "TRACKING", id] => {
                if !fixture.users.is_empty()
                    && let Some(error) = session.auth_error()
                {
                    writeln!(stream, "{error}")?;
                } else if fixture.sticky_pending {
                    writeln!(stream, "PENDING")?;
                } else if session.issued.iter().any(|issued| issued == id) {
                    writeln!(stream, "SUCCESS")?;
                } else {
                    writeln!(stream, "ERR UNKNOWN")?;
                }
            }
            ["INSTCMD", ups, command] | ["INSTCMD", ups, command, _] => {
                if !fixture.users.is_empty()
                    && let Some(error) = session.auth_error()
                {
                    writeln!(stream, "{error}")?;
                } else if !ups.eq_ignore_ascii_case(&fixture.ups_name) {
                    writeln!(stream, "ERR UNKNOWN-UPS")?;
                } else if !fixture.users.is_empty()
                    && !command_allowed(fixture, session.username.as_deref(), command)
                {
                    writeln!(stream, "ERR ACCESS-DENIED")?;
                } else if !fixture.commands.iter().any(|known| known == command) {
                    // The drive lacks a handler: upsd reports the command
                    // unsupported instead of forwarding it.
                    writeln!(stream, "ERR CMD-NOT-SUPPORTED")?;
                } else {
                    if *command == "test.battery.start.quick" {
                        fixture.pending_results.clear();
                        fixture
                            .pending_results
                            .extend(fixture.test_result_sequence.iter().cloned());
                    } else if *command == "test.battery.stop" {
                        fixture.pending_results.clear();
                        fixture
                            .variables
                            .insert("ups.test.result".to_owned(), "Aborted".to_owned());
                    }
                    if session.tracking {
                        session.next_tracking += 1;
                        let id = format!("fake-tracking-{}", session.next_tracking);
                        session.issued.push(id.clone());
                        writeln!(stream, "OK TRACKING {id}")?;
                    } else {
                        writeln!(stream, "OK")?;
                    }
                }
            }
            _ => {
                // The fake deliberately rejects every other write or
                // power-affecting command.
                writeln!(stream, "ERR UNKNOWN-COMMAND")?;
            }
        }
        stream.flush()?;
    }
}

fn command_allowed(fixture: &FakeUpsdFixture, username: Option<&str>, command: &str) -> bool {
    username
        .and_then(|name| fixture.users.get(name))
        .is_some_and(|user| {
            user.instcmds
                .iter()
                .any(|allowed| allowed == "ALL" || allowed == command)
        })
}

/// Split one protocol line like parseconf: whitespace separates fields,
/// double quotes group words, and a backslash escapes the next character.
fn split_command(command: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars();
    let mut in_quotes = false;
    let mut in_field = false;
    loop {
        match chars.next() {
            None => {
                if in_quotes {
                    return None;
                }
                if in_field {
                    fields.push(std::mem::take(&mut current));
                }
                return Some(fields);
            }
            Some('\\') => {
                current.push(chars.next()?);
                in_field = true;
            }
            Some('"') => {
                in_quotes = !in_quotes;
                in_field = true;
            }
            Some(ch) if ch.is_whitespace() && !in_quotes => {
                if in_field {
                    fields.push(std::mem::take(&mut current));
                    in_field = false;
                }
            }
            Some(ch) => {
                current.push(ch);
                in_field = true;
            }
        }
    }
}

fn read_command<R: BufRead>(reader: &mut R) -> io::Result<Option<String>> {
    let mut command = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if command.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial fake upsd command",
                ))
            };
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let has_newline = available[consumed - 1] == b'\n';
        let content_length = consumed - usize::from(has_newline);
        if command.len() + content_length > MAX_COMMAND_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fake upsd command exceeded limit",
            ));
        }
        command.extend_from_slice(&available[..content_length]);
        reader.consume(consumed);
        if has_newline {
            if command.last() == Some(&b'\r') {
                command.pop();
            }
            let text = String::from_utf8(command)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 command"))?;
            if text.contains(['\r', '\n']) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "embedded line break in fake upsd command",
                ));
            }
            return Ok(Some(text));
        }
    }
}

fn quote_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn validate_text(value: &str) -> Result<(), String> {
    if value.chars().any(char::is_control) {
        Err("fixture text cannot contain control characters".into())
    } else {
        Ok(())
    }
}

fn valid_token(value: &str, max_length: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_var_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upsd::{TrackingState, UpsdClient, UpsdError};
    use std::thread;

    #[test]
    fn in_process_fake_upsd_serves_scripted_variables() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut fixture = FakeUpsdFixture {
            ups_name: "demo".into(),
            description: "Scripted UPS".into(),
            variables: BTreeMap::from([
                ("ups.status".into(), "OL CHRG".into()),
                ("battery.charge".into(), "100".into()),
            ]),
            commands: vec!["shutdown.return".into()],
            clients: vec!["127.0.0.1".into()],
            ..Default::default()
        };
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(stream, &mut fixture).unwrap();
        });
        let mut client = UpsdClient::connect("127.0.0.1", port, "demo").unwrap();
        let vars = client.list_vars().unwrap();
        let version = client.version().unwrap();
        let cmds = client.list_cmds().unwrap();
        let clients = client.list_clients().unwrap();
        drop(client);
        server.join().unwrap();
        assert_eq!(vars["ups.status"], "OL CHRG");
        assert_eq!(vars["battery.charge"], "100");
        assert!(version.contains("2.8.1"), "{version}");
        assert_eq!(cmds, vec!["shutdown.return"]);
        assert_eq!(clients, vec!["127.0.0.1"]);
    }

    #[test]
    fn fake_fixture_rejects_text_that_could_break_protocol_lines() {
        let fixture = FakeUpsdFixture {
            ups_name: "demo".into(),
            description: "fine".into(),
            variables: BTreeMap::from([("ups.status".into(), "OL\nFSD".into())]),
            commands: Vec::new(),
            clients: Vec::new(),
            ..Default::default()
        };
        assert!(fixture.validate().is_err());
    }

    fn authed_fixture() -> FakeUpsdFixture {
        FakeUpsdFixture {
            ups_name: "demo".into(),
            description: "Authed UPS".into(),
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
            clients: Vec::new(),
            users: BTreeMap::from([(
                "nut-operator".into(),
                FakeUser {
                    // A space exercises the quoted-password path.
                    password: "op secret".into(),
                    instcmds: vec![
                        "beeper.mute".into(),
                        "beeper.enable".into(),
                        "test.battery.start.quick".into(),
                        "test.battery.stop".into(),
                    ],
                },
            )]),
            test_result_sequence: vec!["Test in progress".into(), "OK".into()],
            ..Default::default()
        }
    }

    fn serve_once(mut fixture: FakeUpsdFixture) -> (u16, thread::JoinHandle<io::Result<()>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(stream, &mut fixture)
        });
        // The server thread serves the single accepted connection; each test
        // ends it by dropping or logging out its client, then joins.
        (port, server)
    }

    fn server_error_code(error: &UpsdError) -> &str {
        match error {
            UpsdError::Server { code, .. } => code.as_str(),
            _ => panic!("expected a server error, got {error:?}"),
        }
    }

    #[test]
    fn split_command_groups_quotes_and_escapes() {
        assert_eq!(
            split_command("PASSWORD \"op secret\"").unwrap(),
            vec!["PASSWORD", "op secret"]
        );
        assert_eq!(
            split_command("PASSWORD say\\\"hi\\\\").unwrap(),
            vec!["PASSWORD", "say\"hi\\"]
        );
        assert_eq!(
            split_command("  INSTCMD   demo   beeper.mute  ").unwrap(),
            vec!["INSTCMD", "demo", "beeper.mute"]
        );
        assert!(split_command("PASSWORD \"unterminated").is_none());
    }

    #[test]
    fn authenticated_tracking_flow_reports_success() {
        let (port, server) = serve_once(authed_fixture());
        let mut client = UpsdClient::connect("127.0.0.1", port, "demo").unwrap();
        client.login("nut-operator", "op secret").unwrap();
        assert!(client.set_tracking(true).unwrap());
        let id = client.instcmd("beeper.mute").unwrap().expect("tracking id");
        assert_eq!(client.get_tracking(&id).unwrap(), TrackingState::Success);
        assert_eq!(
            client.get_var("ups.beeper.status").unwrap(),
            Some("enabled".into())
        );
        assert_eq!(client.get_var("no.such.var").unwrap(), None);
        client.logout();
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn operator_allowlist_rejects_power_commands_before_driver_support() {
        let (port, server) = serve_once(authed_fixture());
        let mut client = UpsdClient::connect("127.0.0.1", port, "demo").unwrap();
        // Unknown users never authenticate.
        let error = client.login("intruder", "op secret").unwrap_err();
        assert_eq!(server_error_code(&error), "ACCESS-DENIED");
        // A wrong password is rejected without further detail.
        let error = client.login("nut-operator", "wrong").unwrap_err();
        assert_eq!(server_error_code(&error), "ACCESS-DENIED");
        drop(client);
        server.join().unwrap().unwrap();

        let (port, server) = serve_once(authed_fixture());
        let mut client = UpsdClient::connect("127.0.0.1", port, "demo").unwrap();
        client.login("nut-operator", "op secret").unwrap();
        assert!(client.set_tracking(true).unwrap());
        // Not in the allowlist: denied even though the driver lacks it too.
        let error = client.instcmd("load.off").unwrap_err();
        assert_eq!(server_error_code(&error), "ACCESS-DENIED");
        // Allowed but unsupported by the driver: forwarded, then refused.
        let error = client.instcmd("beeper.enable").unwrap_err();
        assert_eq!(server_error_code(&error), "CMD-NOT-SUPPORTED");
        client.logout();
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn battery_test_progression_surfaces_scripted_results() {
        let (port, server) = serve_once(authed_fixture());
        let mut client = UpsdClient::connect("127.0.0.1", port, "demo").unwrap();
        client.login("nut-operator", "op secret").unwrap();
        assert!(client.set_tracking(true).unwrap());
        client.instcmd("test.battery.start.quick").unwrap();
        assert_eq!(
            client.get_var("ups.test.result").unwrap(),
            Some("Test in progress".into())
        );
        assert_eq!(
            client.get_var("ups.test.result").unwrap(),
            Some("OK".into())
        );
        client.instcmd("test.battery.stop").unwrap();
        assert_eq!(
            client.get_var("ups.test.result").unwrap(),
            Some("Aborted".into())
        );
        client.logout();
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn sticky_pending_never_completes_a_tracked_command() {
        let mut fixture = authed_fixture();
        fixture.sticky_pending = true;
        let (port, server) = serve_once(fixture);
        let mut client = UpsdClient::connect("127.0.0.1", port, "demo").unwrap();
        client.login("nut-operator", "op secret").unwrap();
        assert!(client.set_tracking(true).unwrap());
        let id = client.instcmd("beeper.mute").unwrap().expect("tracking id");
        assert_eq!(client.get_tracking(&id).unwrap(), TrackingState::Pending);
        assert_eq!(
            client.get_tracking("no-such-id").unwrap(),
            TrackingState::Pending
        );
        drop(client);
        server.join().unwrap().unwrap();
    }
}
