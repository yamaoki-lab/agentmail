use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const MAX_LINE: usize = 64 * 1024;
const MAX_HEADER: usize = 256 * 1024;

pub struct Imap {
    reader: BufReader<TcpStream>,
    tag: u32,
    literal_limit: usize,
}

pub struct Response {
    pub line: String,
    pub literal: Option<Vec<u8>>,
}

impl Imap {
    pub fn login(address: SocketAddr, user: &str, password: &str) -> io::Result<Self> {
        let socket = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
        socket.set_read_timeout(Some(Duration::from_secs(10)))?;
        socket.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut session = Self {
            reader: BufReader::new(socket),
            tag: 0,
            literal_limit: MAX_HEADER,
        };
        if !session.line()?.starts_with("* OK") {
            return Err(invalid("unexpected IMAP greeting"));
        }
        session.command(&format!("LOGIN {} {}", quoted(user)?, quoted(password)?))?;
        Ok(session)
    }

    pub fn set_literal_limit(&mut self, size: usize) {
        self.literal_limit = size;
    }

    pub fn wait_for_change(&mut self) -> io::Result<()> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(25 * 60)))?;
        let result = loop {
            let line = self.line()?;
            if line.starts_with("* BYE") || line.contains("[NOTIFICATIONOVERFLOW]") {
                break Err(invalid("IMAP subscription ended"));
            }
            if line.starts_with("* STATUS INBOX ") || line.starts_with("* STATUS \"INBOX\" ") {
                break Ok(());
            }
        };
        self.reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(10)))?;
        result
    }

    pub fn command(&mut self, command: &str) -> io::Result<Vec<Response>> {
        self.tag += 1;
        let tag = format!("a{} ", self.tag);
        write!(self.reader.get_mut(), "{tag}{command}\r\n")?;
        let mut responses = Vec::new();
        loop {
            let mut line = self.line()?;
            if let Some(status) = line.strip_prefix(&tag) {
                return if status == "OK" || status.starts_with("OK ") {
                    Ok(responses)
                } else {
                    // Avoid printing server responses that might include credentials.
                    Err(invalid(format!(
                        "IMAP rejected {}",
                        command.split_whitespace().next().unwrap_or("command")
                    )))
                };
            }
            if line.starts_with("* BYE") {
                return Err(invalid("Dovecot closed the IMAP session"));
            }
            let mut literal = None;
            if let Some(length) = literal_length(&line, self.literal_limit)? {
                let mut bytes = vec![0; length];
                self.reader.read_exact(&mut bytes)?;
                line.push_str(&self.line()?);
                if literal_length(&line, self.literal_limit)?.is_some() {
                    return Err(invalid("unexpected multiple IMAP literals"));
                }
                literal = Some(bytes);
            }
            responses.push(Response { line, literal });
            if responses.len() > 4096 {
                return Err(invalid("too many IMAP response lines"));
            }
        }
    }

    fn line(&mut self) -> io::Result<String> {
        let mut bytes = Vec::new();
        let size = (&mut self.reader)
            .take((MAX_LINE + 1) as u64)
            .read_until(b'\n', &mut bytes)?;
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "IMAP connection closed",
            ));
        }
        if size > MAX_LINE || !bytes.ends_with(b"\r\n") {
            return Err(invalid("invalid or oversized IMAP response line"));
        }
        bytes.truncate(size - 2);
        String::from_utf8(bytes).map_err(|_| invalid("invalid UTF-8 in IMAP response line"))
    }
}

fn literal_length(line: &str, limit: usize) -> io::Result<Option<usize>> {
    let Some((_, value)) = line
        .strip_suffix('}')
        .and_then(|line| line.rsplit_once('{'))
    else {
        return Ok(None);
    };
    let length: usize = value
        .parse()
        .map_err(|_| invalid("invalid IMAP literal size"))?;
    if length > limit {
        return Err(invalid("IMAP literal exceeds size limit"));
    }
    Ok(Some(length))
}

fn quoted(text: &str) -> io::Result<String> {
    if !text.is_ascii() || text.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(invalid("invalid character in IMAP credentials"));
    }
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

pub fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
