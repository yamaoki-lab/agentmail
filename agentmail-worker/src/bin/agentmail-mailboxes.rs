use agentmail_worker::imap::{invalid, Imap};
use agentmail_worker::runtime::{
    atomic_write, group, random_hex, run, unix_lock, valid_identity, TemporaryDirectory,
};
use clap::{Parser, Subcommand};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, DirBuilder};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::{chown, DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const DOMAIN: &str = "agentmail.test";
const REGISTRY: &str = "/data/.agentmail-identities";
const ACCOUNTS: &str = "/run/agentmail-lab/accounts.json";
const POSTFIX: &str = "/etc/postfix/mailboxes";
const DOVECOT: &str = "/etc/dovecot/lab-users";
type Accounts = BTreeMap<String, String>;

#[derive(Parser)]
#[command(about = "Provision and verify local Postfix and Dovecot mailboxes")]
struct Args {
    #[command(subcommand)]
    operation: Operation,
}

#[derive(Subcommand)]
enum Operation {
    Sync,
    List,
    Create { identity: String },
    Ready,
}

fn registered(path: &Path) -> io::Result<BTreeSet<String>> {
    let mut names = BTreeSet::from(["alpha".to_owned(), "beta".to_owned()]);
    match fs::read_to_string(path) {
        Ok(contents) => {
            for name in contents.lines() {
                if !valid_identity(name) {
                    return Err(invalid("invalid mailbox in the registry"));
                }
                names.insert(name.to_owned());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(names)
}

fn accounts(path: &Path) -> io::Result<Accounts> {
    match fs::read(path) {
        Ok(contents) => Ok(serde_json::from_slice(&contents)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Accounts::new()),
        Err(error) => Err(error),
    }
}

fn sync(names: &BTreeSet<String>) -> io::Result<Accounts> {
    let previous = accounts(Path::new(ACCOUNTS))?;
    let mut current = Accounts::new();
    let mut users = String::new();
    let mut recipients = String::new();
    for name in names {
        let address = format!("{name}@{DOMAIN}");
        let password = match previous.get(&address).filter(|value| !value.is_empty()) {
            Some(password) => password.clone(),
            None => random_hex(24)?,
        };
        if !password
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(invalid("unexpected characters in local IMAP credentials"));
        }
        let home = Path::new("/data").join(name);
        DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&home)?;
        chown(&home, Some(5000), Some(5000))?;
        writeln!(
            users,
            "{address}:{{PLAIN}}{password}:5000:5000::{}::",
            home.display()
        )
        .unwrap();
        writeln!(recipients, "{address} {name}/").unwrap();
        current.insert(address, password);
    }

    let postfix = Path::new(POSTFIX);
    let stage = TemporaryDirectory::new(postfix.parent().unwrap(), ".agentmail-map.")?;
    let table = stage.path().join("mailboxes");
    fs::write(&table, recipients)?;
    fs::set_permissions(&table, fs::Permissions::from_mode(0o644))?;
    run(Command::new("postmap").arg(format!("hash:{}", table.display())))?;
    let database = table.with_extension("db");
    fs::set_permissions(&database, fs::Permissions::from_mode(0o644))?;
    atomic_write(
        Path::new(DOVECOT),
        users.as_bytes(),
        0o640,
        Some(group("dovecot")?),
    )?;
    fs::rename(&table, postfix)?;
    fs::rename(&database, postfix.with_extension("db"))?;
    atomic_write(
        Path::new(ACCOUNTS),
        &serde_json::to_vec(&current)?,
        0o600,
        None,
    )?;
    Ok(current)
}

fn smtp_response(stream: &mut BufReader<TcpStream>, code: &[u8; 3]) -> io::Result<()> {
    for _ in 0..64 {
        let mut response = Vec::new();
        stream.take(4097).read_until(b'\n', &mut response)?;
        if response.len() > 4096 || !response.ends_with(b"\r\n") || !response.starts_with(code) {
            return Err(invalid("unexpected SMTP response"));
        }
        match response.get(3) {
            Some(b' ') => return Ok(()),
            Some(b'-') => {}
            _ => return Err(invalid("invalid SMTP response")),
        }
    }
    Err(invalid("too many SMTP response lines"))
}

fn smtp_probe(recipient: Option<&str>) -> io::Result<()> {
    let socket =
        TcpStream::connect_timeout(&"127.0.0.1:25".parse().unwrap(), Duration::from_secs(2))?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut stream = BufReader::new(socket);
    smtp_response(&mut stream, b"220")?;
    stream.get_mut().write_all(b"EHLO localhost\r\n")?;
    smtp_response(&mut stream, b"250")?;
    if let Some(recipient) = recipient {
        write!(stream.get_mut(), "MAIL FROM:<alpha@{DOMAIN}>\r\n")?;
        smtp_response(&mut stream, b"250")?;
        write!(stream.get_mut(), "RCPT TO:<{recipient}>\r\n")?;
        smtp_response(&mut stream, b"250")?;
    } else {
        stream.get_mut().write_all(b"NOOP\r\n")?;
        smtp_response(&mut stream, b"250")?;
    }
    Ok(())
}

fn imap_probe(address: &str, accounts: &Accounts) -> io::Result<()> {
    let password = accounts
        .get(address)
        .ok_or_else(|| invalid("mailbox has no IMAP credentials"))?;
    Imap::login("127.0.0.1:143".parse().unwrap(), address, password)?.command("EXAMINE INBOX")?;
    Ok(())
}

fn retry(timeout: Duration, probe: impl Fn() -> io::Result<()>) -> io::Result<()> {
    let started = Instant::now();
    loop {
        match probe() {
            Ok(()) => return Ok(()),
            Err(error) if started.elapsed() >= timeout => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(500)),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let operation = Args::parse().operation;
    if matches!(operation, Operation::Ready) {
        let accounts = accounts(Path::new(ACCOUNTS))?;
        retry(Duration::from_secs(30), || {
            smtp_probe(None)?;
            imap_probe("alpha@agentmail.test", &accounts)?;
            imap_probe("beta@agentmail.test", &accounts)
        })?;
        return Ok(());
    }
    let registry = Path::new(REGISTRY);
    let _lock = unix_lock(Path::new("/data/.agentmail-identities.lock"), libc::LOCK_EX)?;
    let mut names = registered(registry)?;
    match operation {
        Operation::List => {
            for name in names {
                println!("{name}@{DOMAIN}");
            }
        }
        Operation::Sync => {
            sync(&names)?;
        }
        Operation::Create { identity } => {
            if !valid_identity(&identity) {
                return Err(invalid(
                    "use 1–32 lowercase letters, digits, or hyphens, starting with a letter",
                )
                .into());
            }
            if names.insert(identity.clone()) {
                let registry_data = names.iter().cloned().collect::<Vec<_>>().join("\n") + "\n";
                atomic_write(registry, registry_data.as_bytes(), 0o600, None)?;
            }
            let accounts = sync(&names)?;
            run(Command::new("doveadm").arg("reload"))?;
            let address = format!("{identity}@{DOMAIN}");
            retry(Duration::from_secs(15), || {
                imap_probe(&address, &accounts)?;
                smtp_probe(Some(&address))
            })?;
            println!("READY: {address} accepted by SMTP and readable over IMAP");
        }
        Operation::Ready => unreachable!(),
    }
    Ok(())
}
