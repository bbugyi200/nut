use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

const MAX_COMMAND_BYTES: usize = 1024;
const MAX_VARIABLES: usize = 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeUpsdFixture {
    pub ups_name: String,
    pub description: String,
    pub variables: BTreeMap<String, String>,
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
        Ok(())
    }
}

/// Run a read-only scripted upsd that accepts connections only from loopback clients.
pub fn run_fake_upsd(listen: &str, fixture_path: &Path) -> Result<(), String> {
    let fixture = FakeUpsdFixture::read(fixture_path)?;
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
                if let Err(error) = serve_client(stream, &fixture) {
                    eprintln!("fake upsd client ended: {error}");
                }
            }
            Err(error) => return Err(format!("cannot accept fake upsd client: {error}")),
        }
    }
    Ok(())
}

/// Serve one fake upsd connection. Exposed for the in-process scripted client test.
pub fn serve_client(mut stream: TcpStream, fixture: &FakeUpsdFixture) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    writeln!(stream, "Network UPS Tools upsd 2.8.1 - scripted fake")?;
    let mut reader = BufReader::new(stream.try_clone()?);
    loop {
        let Some(command) = read_command(&mut reader)? else {
            return Ok(());
        };
        let fields = command.split_ascii_whitespace().collect::<Vec<_>>();
        match fields.as_slice() {
            ["VER"] => writeln!(stream, "upsd 2.8.1 - scripted fake")?,
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
                if let Some(value) = fixture.variables.get(&name.to_ascii_lowercase()) {
                    writeln!(
                        stream,
                        "VAR {} {} \"{}\"",
                        fixture.ups_name,
                        name.to_ascii_lowercase(),
                        quote_value(value)
                    )?;
                } else {
                    writeln!(stream, "ERR UNKNOWN-VAR {name}")?;
                }
            }
            ["LIST", "VAR", _] | ["GET", "VAR", _, _] => {
                writeln!(stream, "ERR UNKNOWN-UPS")?;
            }
            _ => {
                // The fake deliberately rejects every write or power-affecting command.
                writeln!(stream, "ERR UNKNOWN-COMMAND")?;
            }
        }
        stream.flush()?;
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
    use crate::upsd::UpsdClient;
    use std::thread;

    #[test]
    fn in_process_fake_upsd_serves_scripted_variables() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let fixture = FakeUpsdFixture {
            ups_name: "demo".into(),
            description: "Scripted UPS".into(),
            variables: BTreeMap::from([
                ("ups.status".into(), "OL CHRG".into()),
                ("battery.charge".into(), "100".into()),
            ]),
        };
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(stream, &fixture).unwrap();
        });
        let vars = UpsdClient::connect("127.0.0.1", port, "demo")
            .unwrap()
            .list_vars()
            .unwrap();
        server.join().unwrap();
        assert_eq!(vars["ups.status"], "OL CHRG");
        assert_eq!(vars["battery.charge"], "100");
    }

    #[test]
    fn fake_fixture_rejects_text_that_could_break_protocol_lines() {
        let fixture = FakeUpsdFixture {
            ups_name: "demo".into(),
            description: "fine".into(),
            variables: BTreeMap::from([("ups.status".into(), "OL\nFSD".into())]),
        };
        assert!(fixture.validate().is_err());
    }
}
