use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use thiserror::Error;

/// Read LIST UPS within one wall-clock budget, including connect and slow/drip
/// responses. Numeric loopback addresses avoid unbounded DNS during completion.
pub fn list_ups(
    address: &str,
    port: u16,
    budget: Duration,
) -> Result<Vec<(String, String)>, UpsdError> {
    use std::io::Read;
    use std::time::Instant;
    let started = Instant::now();
    let ip: IpAddr = if address.eq_ignore_ascii_case("localhost") {
        IpAddr::from([127, 0, 0, 1])
    } else {
        address
            .parse()
            .map_err(|_| UpsdError::NonLoopbackAddress(address.into()))?
    };
    if !ip.is_loopback() {
        return Err(UpsdError::NonLoopbackAddress(address.into()));
    }
    let failure = |error: io::Error| UpsdError::Protocol(format!("LIST UPS: {error}"));
    let remaining = || {
        budget
            .checked_sub(started.elapsed())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| failure(io::ErrorKind::TimedOut.into()))
    };
    let mut stream =
        TcpStream::connect_timeout(&SocketAddr::new(ip, port), remaining()?).map_err(failure)?;
    stream
        .set_write_timeout(Some(remaining()?))
        .map_err(failure)?;
    stream.write_all(b"LIST UPS\n").map_err(failure)?;
    let mut bytes = Vec::new();
    let mut rows = Vec::new();
    let mut begun = false;
    let mut line_count = 0;
    let mut total = 0;
    loop {
        stream
            .set_read_timeout(Some(remaining()?))
            .map_err(failure)?;
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).map_err(failure)?;
        if count == 0 {
            return Err(failure(io::ErrorKind::UnexpectedEof.into()));
        }
        total += count;
        if total > MAX_RESPONSE_BYTES {
            return Err(UpsdError::Protocol("LIST UPS response too large".into()));
        }
        bytes.extend_from_slice(&chunk[..count]);
        while let Some(end) = bytes.iter().position(|byte| *byte == b'\n') {
            line_count += 1;
            if end > MAX_LINE_BYTES || line_count > MAX_RESPONSE_LINES {
                return Err(UpsdError::Protocol("LIST UPS limits exceeded".into()));
            }
            let line = std::str::from_utf8(&bytes[..end])
                .map_err(|error| UpsdError::Protocol(error.to_string()))?
                .trim_end_matches('\r');
            if let Some(error) = parse_server_error(line) {
                return Err(error);
            }
            if !begun {
                if line == "BEGIN LIST UPS" {
                    begun = true;
                } else if line_count != 1 || !line.starts_with("Network UPS Tools upsd ") {
                    return Err(UpsdError::Protocol("expected BEGIN LIST UPS".into()));
                }
            } else if line == "END LIST UPS" {
                return Ok(rows);
            } else if let Some((name, description)) = line
                .strip_prefix("UPS ")
                .and_then(|line| line.split_once(' '))
            {
                validate_ups_name(name)?;
                if rows.iter().any(|(existing, _)| existing == name) {
                    return Err(UpsdError::Protocol("duplicate UPS name".into()));
                }
                rows.push((name.to_owned(), parse_quoted_value(description)?));
            } else {
                return Err(UpsdError::Protocol("expected UPS row".into()));
            }
            bytes.drain(..=end);
        }
        if bytes.len() > MAX_LINE_BYTES {
            return Err(UpsdError::Protocol("LIST UPS line too large".into()));
        }
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_LINE_BYTES: usize = 4 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_LINES: usize = 1024;

#[derive(Debug, Error)]
pub enum UpsdError {
    #[error("invalid UPS name {0:?}")]
    InvalidUpsName(String),
    #[error("invalid upsd address {0:?}; only loopback addresses are allowed")]
    NonLoopbackAddress(String),
    #[error("cannot resolve loopback upsd address {address}:{port}: {source}")]
    Resolve {
        address: String,
        port: u16,
        #[source]
        source: io::Error,
    },
    #[error("cannot connect to upsd at {address}:{port}: {source}")]
    Connect {
        address: String,
        port: u16,
        #[source]
        source: io::Error,
    },
    #[error("upsd protocol error: {0}")]
    Protocol(String),
    #[error("upsd returned {code}: {message}")]
    Server { code: String, message: String },
}

/// A bounded, read-only RFC 9271 client for one UPS on a loopback upsd.
pub struct UpsdClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    ups: String,
    response_bytes: usize,
    response_lines: usize,
    greeting_seen: bool,
    greeting: Option<String>,
}

impl UpsdClient {
    pub fn connect(address: &str, port: u16, ups: &str) -> Result<Self, UpsdError> {
        validate_ups_name(ups)?;
        let addresses = resolve_loopback(address, port)?;
        let mut last_error = None;
        let mut connected = None;
        for socket_address in addresses {
            match TcpStream::connect_timeout(&socket_address, CONNECT_TIMEOUT) {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        let stream = connected.ok_or_else(|| UpsdError::Connect {
            address: address.to_owned(),
            port,
            source: last_error.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "no loopback address resolved",
                )
            }),
        })?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))
            .map_err(|source| UpsdError::Connect {
                address: address.to_owned(),
                port,
                source,
            })?;
        let reader = BufReader::with_capacity(
            4096,
            stream.try_clone().map_err(|source| UpsdError::Connect {
                address: address.to_owned(),
                port,
                source,
            })?,
        );
        Ok(Self {
            reader,
            writer: stream,
            ups: ups.to_owned(),
            response_bytes: 0,
            response_lines: 0,
            greeting_seen: false,
            greeting: None,
        })
    }

    pub fn greeting(&self) -> Option<&str> {
        self.greeting.as_deref()
    }

    /// Read all variables in one bounded request. This client never sends write commands.
    pub fn list_vars(&mut self) -> Result<BTreeMap<String, String>, UpsdError> {
        self.write_command(&format!("LIST VAR {}", self.ups))?;
        let expected_begin = format!("BEGIN LIST VAR {}", self.ups);
        let expected_end = format!("END LIST VAR {}", self.ups);
        let first = self.read_response_line()?;
        if let Some(error) = parse_server_error(&first) {
            return Err(error);
        }
        if first != expected_begin {
            return Err(UpsdError::Protocol(format!(
                "expected {expected_begin:?}, received {first:?}"
            )));
        }

        let mut vars = BTreeMap::new();
        loop {
            let line = self.read_response_line()?;
            if line == expected_end {
                return Ok(vars);
            }
            if let Some(error) = parse_server_error(&line) {
                return Err(error);
            }
            let (ups, name, value) = parse_var_response(&line)?;
            if !ups.eq_ignore_ascii_case(&self.ups) {
                return Err(UpsdError::Protocol(format!(
                    "response UPS name {ups:?} does not match {:?}",
                    self.ups
                )));
            }
            if !is_valid_var_name(&name) {
                return Err(UpsdError::Protocol(format!(
                    "invalid variable name in response: {name:?}"
                )));
            }
            let key = name.to_ascii_lowercase();
            if vars.insert(key.clone(), value).is_some() {
                return Err(UpsdError::Protocol(format!(
                    "duplicate variable in response: {key}"
                )));
            }
        }
    }

    /// Query the daemon version. Call after another command so a connect
    /// banner is not mistaken for this reply on servers that omit the banner.
    pub fn version(&mut self) -> Result<String, UpsdError> {
        self.write_command("VER")?;
        let line = self.read_response_line()?;
        if let Some(error) = parse_server_error(&line) {
            return Err(error);
        }
        Ok(line)
    }

    pub fn list_cmds(&mut self) -> Result<Vec<String>, UpsdError> {
        self.list_prefixed("CMD", |line| parse_token_row("CMD", line))
    }

    pub fn list_clients(&mut self) -> Result<Vec<String>, UpsdError> {
        self.list_prefixed("CLIENT", |line| parse_token_row("CLIENT", line))
    }

    fn list_prefixed(
        &mut self,
        kind: &str,
        parse_row: fn(&str) -> Result<(String, String), UpsdError>,
    ) -> Result<Vec<String>, UpsdError> {
        self.write_command(&format!("LIST {kind} {}", self.ups))?;
        let expected_begin = format!("BEGIN LIST {kind} {}", self.ups);
        let expected_end = format!("END LIST {kind} {}", self.ups);
        let first = self.read_response_line()?;
        if let Some(error) = parse_server_error(&first) {
            return Err(error);
        }
        if first != expected_begin {
            return Err(UpsdError::Protocol(format!(
                "expected {expected_begin:?}, received {first:?}"
            )));
        }
        let mut rows = Vec::new();
        loop {
            let line = self.read_response_line()?;
            if line == expected_end {
                return Ok(rows);
            }
            if let Some(error) = parse_server_error(&line) {
                return Err(error);
            }
            let (ups, value) = parse_row(&line)?;
            if !ups.eq_ignore_ascii_case(&self.ups) {
                return Err(UpsdError::Protocol(format!(
                    "response UPS name {ups:?} does not match {:?}",
                    self.ups
                )));
            }
            rows.push(value);
        }
    }

    fn write_command(&mut self, command: &str) -> Result<(), UpsdError> {
        if command.len() > 256 || command.contains(['\n', '\r']) {
            return Err(UpsdError::Protocol(
                "invalid command length or line break".into(),
            ));
        }
        self.writer
            .write_all(command.as_bytes())
            .and_then(|()| self.writer.write_all(b"\n"))
            .and_then(|()| self.writer.flush())
            .map_err(|error| UpsdError::Protocol(format!("cannot send request: {error}")))
    }

    fn read_response_line(&mut self) -> Result<String, UpsdError> {
        loop {
            self.response_lines += 1;
            if self.response_lines > MAX_RESPONSE_LINES {
                return Err(UpsdError::Protocol(format!(
                    "response exceeded {MAX_RESPONSE_LINES} lines"
                )));
            }
            let line = read_bounded_line(
                &mut self.reader,
                &mut self.response_bytes,
                MAX_LINE_BYTES,
                MAX_RESPONSE_BYTES,
            )?;
            if !self.greeting_seen {
                self.greeting_seen = true;
                if line.starts_with("Network UPS Tools upsd ") {
                    self.greeting = Some(line);
                    continue;
                }
            }
            return Ok(line);
        }
    }
}

fn resolve_loopback(address: &str, port: u16) -> Result<Vec<SocketAddr>, UpsdError> {
    if let Ok(ip) = address.parse::<IpAddr>() {
        if !ip.is_loopback() {
            return Err(UpsdError::NonLoopbackAddress(address.to_owned()));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    if !address.eq_ignore_ascii_case("localhost") {
        return Err(UpsdError::NonLoopbackAddress(address.to_owned()));
    }
    let resolved = (address, port)
        .to_socket_addrs()
        .map_err(|source| UpsdError::Resolve {
            address: address.to_owned(),
            port,
            source,
        })?
        .collect::<Vec<_>>();
    if resolved.is_empty() || resolved.iter().any(|socket| !socket.ip().is_loopback()) {
        return Err(UpsdError::NonLoopbackAddress(address.to_owned()));
    }
    Ok(resolved)
}

fn validate_ups_name(ups: &str) -> Result<(), UpsdError> {
    if !ups.is_empty()
        && ups.len() <= 64
        && ups
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        Ok(())
    } else {
        Err(UpsdError::InvalidUpsName(ups.to_owned()))
    }
}

fn is_valid_var_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_')
        })
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    total_bytes: &mut usize,
    max_line_bytes: usize,
    max_total_bytes: usize,
) -> Result<String, UpsdError> {
    let mut line = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .map_err(|error| UpsdError::Protocol(format!("cannot read response: {error}")))?;
        if available.is_empty() {
            return Err(UpsdError::Protocol(
                "connection closed before the response line ended".into(),
            ));
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let content_length = consumed - usize::from(available[consumed - 1] == b'\n');
        if line.len() + content_length > max_line_bytes {
            return Err(UpsdError::Protocol(format!(
                "response line exceeded {max_line_bytes} bytes"
            )));
        }
        if *total_bytes + consumed > max_total_bytes {
            return Err(UpsdError::Protocol(format!(
                "response exceeded {max_total_bytes} bytes"
            )));
        }
        line.extend_from_slice(&available[..content_length]);
        let complete = available[consumed - 1] == b'\n';
        reader.consume(consumed);
        *total_bytes += consumed;
        if complete {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return String::from_utf8(line)
                .map_err(|_| UpsdError::Protocol("response was not valid UTF-8".into()));
        }
    }
}

fn parse_server_error(line: &str) -> Option<UpsdError> {
    let rest = line.strip_prefix("ERR ")?;
    let (code, message) = rest
        .split_once(' ')
        .map_or((rest, ""), |(code, message)| (code, message));
    Some(UpsdError::Server {
        code: code.to_owned(),
        message: message.to_owned(),
    })
}

fn parse_token_row(kind: &str, line: &str) -> Result<(String, String), UpsdError> {
    let mut fields = line.splitn(3, ' ');
    if fields.next() != Some(kind) {
        return Err(UpsdError::Protocol(format!(
            "expected a {kind} response, received {line:?}"
        )));
    }
    let ups = fields
        .next()
        .ok_or_else(|| UpsdError::Protocol(format!("malformed {kind} response: {line:?}")))?;
    let value = fields
        .next()
        .ok_or_else(|| UpsdError::Protocol(format!("malformed {kind} response: {line:?}")))?;
    Ok((ups.to_owned(), value.to_owned()))
}

fn parse_var_response(line: &str) -> Result<(String, String, String), UpsdError> {
    let mut fields = line.splitn(4, ' ');
    if fields.next() != Some("VAR") {
        return Err(UpsdError::Protocol(format!(
            "expected a VAR response, received {line:?}"
        )));
    }
    let ups = fields
        .next()
        .ok_or_else(|| UpsdError::Protocol(format!("malformed VAR response: {line:?}")))?;
    let name = fields
        .next()
        .ok_or_else(|| UpsdError::Protocol(format!("malformed VAR response: {line:?}")))?;
    let value = fields
        .next()
        .ok_or_else(|| UpsdError::Protocol(format!("malformed VAR response: {line:?}")))?;
    Ok((ups.to_owned(), name.to_owned(), parse_quoted_value(value)?))
}

fn parse_quoted_value(input: &str) -> Result<String, UpsdError> {
    let bytes = input.as_bytes();
    if bytes.first() != Some(&b'"') {
        return Err(UpsdError::Protocol(format!(
            "expected a quoted value, received {input:?}"
        )));
    }
    let mut value = Vec::new();
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                if index + 1 != bytes.len() {
                    return Err(UpsdError::Protocol(format!(
                        "unexpected trailing data after quoted value: {input:?}"
                    )));
                }
                return String::from_utf8(value)
                    .map_err(|_| UpsdError::Protocol("quoted value was not valid UTF-8".into()));
            }
            b'\\' => {
                index += 1;
                let escaped = *bytes.get(index).ok_or_else(|| {
                    UpsdError::Protocol(format!("unterminated escape in value: {input:?}"))
                })?;
                if !matches!(escaped, b'\\' | b'"') {
                    return Err(UpsdError::Protocol(format!(
                        "unsupported escape in value: {input:?}"
                    )));
                }
                value.push(escaped);
            }
            byte if byte.is_ascii_control() => {
                return Err(UpsdError::Protocol(
                    "quoted value contained a control character".into(),
                ));
            }
            byte => value.push(byte),
        }
        index += 1;
    }
    Err(UpsdError::Protocol(format!(
        "unterminated quoted value: {input:?}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn parses_escaped_quotes_and_backslashes() {
        let encoded = "\"say \\\"hello\\\" and slash \\\\ ok\"";
        assert_eq!(
            parse_quoted_value(encoded).unwrap(),
            r#"say "hello" and slash \ ok"#
        );
    }

    #[test]
    fn rejects_bad_quoting_and_control_bytes() {
        assert!(parse_quoted_value("unquoted").is_err());
        assert!(parse_quoted_value("\"unterminated").is_err());
        assert!(parse_quoted_value("\"line\nfeed\"").is_err());
    }

    #[test]
    fn rejects_non_loopback_and_unsafe_ups_names() {
        assert!(matches!(
            resolve_loopback("192.0.2.1", 3493),
            Err(UpsdError::NonLoopbackAddress(_))
        ));
        assert!(validate_ups_name("cp1500; FSD other").is_err());
        assert!(is_valid_var_name("driver.flag.allow_killpower"));
    }

    #[test]
    fn scripted_fake_server_uses_bounded_read_protocol() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let vars = BTreeMap::from([
            ("ups.status".to_owned(), "OL CHRG".to_owned()),
            ("battery.runtime".to_owned(), "1550".to_owned()),
        ]);
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"Network UPS Tools upsd 2.8.1 - fake\n")
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut command = String::new();
            reader.read_line(&mut command).unwrap();
            assert_eq!(command.trim_end(), "LIST VAR cp1500");
            writeln!(stream, "BEGIN LIST VAR cp1500").unwrap();
            for (name, value) in vars {
                writeln!(stream, "VAR cp1500 {name} \"{value}\"").unwrap();
            }
            writeln!(stream, "END LIST VAR cp1500").unwrap();
        });
        let vars = UpsdClient::connect("127.0.0.1", port, "cp1500")
            .unwrap()
            .list_vars()
            .expect("LIST VAR");
        worker.join().unwrap();
        assert_eq!(vars["ups.status"], "OL CHRG");
        assert_eq!(vars["battery.runtime"], "1550");
    }
}
