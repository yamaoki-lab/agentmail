use agentmail_worker::imap::{invalid, Imap};
use agentmail_worker::{Metadata, Snapshot};
use mailparse::MailHeaderMap;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const MAX_METADATA: usize = 20;
const SUBSCRIBE: &str = "NOTIFY SET (MAILBOXES INBOX (MessageNew MessageExpunge FlagChange))";

fn header_text(value: String) -> String {
    value
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect()
}

fn snapshot(server: SocketAddr, identity: &str, password: &str) -> io::Result<Snapshot> {
    let mut imap = Imap::login(server, identity, password)?;
    let selected = imap.command("EXAMINE INBOX")?;
    let uid_validity = selected
        .iter()
        .find_map(|r| r.line.split_once("[UIDVALIDITY "))
        .and_then(|(_, value)| value.split_once(']'))
        .and_then(|(value, _)| value.parse().ok())
        .ok_or_else(|| invalid("missing IMAP UIDVALIDITY"))?;
    let result = imap.command("UID SEARCH UNSEEN")?;
    let list = result
        .iter()
        .find_map(|r| {
            (r.line == "* SEARCH" || r.line.starts_with("* SEARCH ")).then(|| &r.line[8..])
        })
        .ok_or_else(|| invalid("missing IMAP SEARCH response"))?;
    let uids = list
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|_| invalid("invalid IMAP UID list"))?;
    let wanted: BTreeSet<_> = uids.iter().rev().take(MAX_METADATA).copied().collect();
    let messages = if wanted.is_empty() {
        Vec::new()
    } else {
        let sequence = wanted
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let responses = imap.command(&format!(
            "UID FETCH {sequence} (UID BODY.PEEK[HEADER.FIELDS (FROM SUBJECT)])"
        ))?;
        let mut messages = Vec::new();
        for response in responses {
            let Some(bytes) = response.literal else {
                continue;
            };
            let uid = response
                .line
                .split_whitespace()
                .collect::<Vec<_>>()
                .windows(2)
                .find_map(|pair| {
                    (pair[0].trim_start_matches('(') == "UID")
                        .then(|| pair[1].trim_end_matches(')').parse::<u32>().ok())
                        .flatten()
                })
                .filter(|uid| wanted.contains(uid))
                .ok_or_else(|| invalid("unexpected UID in IMAP FETCH"))?;
            let (headers, _) = mailparse::parse_headers(&bytes)
                .map_err(|_| invalid("could not parse RFC 5322 headers"))?;
            messages.push(Metadata {
                uid,
                sender: header_text(headers.get_first_value("From").unwrap_or_default()),
                subject: header_text(headers.get_first_value("Subject").unwrap_or_default()),
            });
        }
        messages.sort_by_key(|mail| std::cmp::Reverse(mail.uid));
        messages
    };
    Ok(Snapshot {
        identity: identity.to_owned(),
        uid_validity,
        unread_count: uids.len(),
        messages,
    })
}

fn publish(cache: &Path, snapshot: &Snapshot) -> io::Result<()> {
    let destination = cache.join(format!("{}.json", snapshot.identity));
    let temporary = cache.join(format!(".{}.{}-tmp", snapshot.identity, std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, snapshot)?;
        file.write_all(b"\n")?;
        file.flush()?;
        fs::rename(&temporary, destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn output(cache: Option<&Path>, current: &Snapshot) -> io::Result<()> {
    if let Some(cache) = cache {
        publish(cache, current)?;
        println!("{}: {} unread", current.identity, current.unread_count);
    } else {
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, current)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

fn watch(server: SocketAddr, identity: &str, password: &str, cache: Option<&Path>) -> ! {
    let mut previous = None;
    loop {
        let result = (|| -> io::Result<()> {
            let mut subscription = Imap::login(server, identity, password)?;
            subscription.command(SUBSCRIBE)?;
            loop {
                // Subscribe first so messages arriving during the initial snapshot are observed.
                let current = snapshot(server, identity, password)?;
                if previous.as_ref() != Some(&current) {
                    output(cache, &current)?;
                    previous = Some(current);
                }
                subscription.wait_for_change()?;
            }
        })();
        if let Err(error) = result {
            eprintln!("{identity}: IMAP refresh needed ({error})");
        }
        thread::sleep(Duration::from_secs(3));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let (accounts, cache, reload) = match args.as_slice() {
        [flag, accounts] if flag == "--stream" || flag == "--stream-reload" => {
            (PathBuf::from(accounts), None, flag == "--stream-reload")
        }
        [accounts, cache] => (PathBuf::from(accounts), Some(PathBuf::from(cache)), false),
        _ => return Err(
            "usage: agentmail-worker <accounts.json> <cache-directory> | --stream[-reload] <accounts.json>"
                .into(),
        ),
    };
    let source = File::open(&accounts)?;
    let credentials: BTreeMap<String, String> = serde_json::from_reader(&source)?;
    if reload {
        let original = source.metadata()?;
        thread::spawn(move || {
            // Keep the original inode open so a replacement cannot reuse it.
            let _source = source;
            loop {
                thread::sleep(Duration::from_secs(2));
                if !fs::metadata(&accounts).is_ok_and(|current| {
                    (current.dev(), current.ino()) == (original.dev(), original.ino())
                }) {
                    eprintln!("agentmail-worker: account registry changed, restarting");
                    std::process::exit(75);
                }
            }
        });
    }
    if credentials.is_empty()
        || credentials.keys().any(|key| {
            key.is_empty()
                || key.len() > 128
                || key == "."
                || key == ".."
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".@_-".contains(&b))
        })
    {
        return Err("invalid or empty account list".into());
    }
    if let Some(cache) = &cache {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(cache)?;
    }
    let server = SocketAddr::from(([127, 0, 0, 1], 143));
    thread::scope(|scope| {
        for (identity, password) in &credentials {
            let cache = cache.as_deref();
            scope.spawn(move || watch(server, identity, password, cache));
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    #[test]
    fn snapshot_uses_peek_and_decodes_folded_headers() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut stream = BufReader::new(stream);
            stream.get_mut().write_all(b"* OK test\r\n").unwrap();

            let headers =
                b"From: Alpha <alpha@agentmail.test>\r\nSubject:\r\n =?UTF-8?B?5paw552A?=\r\n\r\n";
            for (request, response) in [
                (
                    "a1 LOGIN \"beta@agentmail.test\" \"secret\"\r\n",
                    "a1 OK logged in\r\n".to_owned(),
                ),
                (
                    "a2 EXAMINE INBOX\r\n",
                    "* OK [UIDVALIDITY 123] valid\r\na2 OK examined\r\n".to_owned(),
                ),
                (
                    "a3 UID SEARCH UNSEEN\r\n",
                    "* SEARCH 7\r\na3 OK done\r\n".to_owned(),
                ),
                (
                    "a4 UID FETCH 7 (UID BODY.PEEK[HEADER.FIELDS (FROM SUBJECT)])\r\n",
                    format!(
                        "* 1 FETCH (UID 7 BODY[HEADER.FIELDS (FROM SUBJECT)] {{{}}}\r\n{}\
                          )\r\na4 OK done\r\n",
                        headers.len(),
                        std::str::from_utf8(headers).unwrap()
                    ),
                ),
            ] {
                let mut line = String::new();
                stream.read_line(&mut line).unwrap();
                assert_eq!(line, request);
                stream.get_mut().write_all(response.as_bytes()).unwrap();
            }
        });
        let snapshot = snapshot(address, "beta@agentmail.test", "secret").unwrap();
        server.join().unwrap();
        assert_eq!(snapshot.uid_validity, 123);
        assert_eq!(snapshot.unread_count, 1);
        assert_eq!(
            snapshot.messages,
            vec![Metadata {
                uid: 7,
                sender: "Alpha <alpha@agentmail.test>".to_owned(),
                subject: "新着".to_owned(),
            }]
        );
    }
}
