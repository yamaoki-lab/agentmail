//! One SSH stream for the Mac and one local cache keyed by conversation.
use super::{invalid, session};
use agentmail_worker::Snapshot;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const SOCKET: &str = "notify.sock";
const MAX: u64 = 1024 * 1024;
const MAX_AUTH: u64 = 16 * 1024;
const MAX_AGE: Duration = Duration::from_secs(12);
const NAMESPACE: &str = "agentmail-watch-v1";
const REFRESH: &str = "refresh";
type Latest = Arc<Mutex<Option<(Instant, BTreeMap<session::Binding, Snapshot>)>>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedSnapshot {
    scope: session::Binding,
    snapshot: Snapshot,
}

fn relay_directory() -> io::Result<PathBuf> {
    let home = env::var_os("HOME").ok_or_else(|| invalid("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local/share/agentmail/watch"))
}

fn pause_file(directory: &Path, uuid: &str) -> PathBuf {
    directory.join(format!("paused-{uuid}"))
}

pub(super) fn set_paused(paused: bool) -> io::Result<()> {
    let scope = session::configured()?
        .filter(|scope| scope.binding.tool == "claude")
        .ok_or_else(|| invalid("run this in a registered Claude conversation"))?;
    let directory = relay_directory()?;
    DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&directory)?;
    let marker = pause_file(&directory, &scope.binding.uuid);
    if paused {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(marker)?;
        println!("Notifications paused for {}", scope.address());
    } else {
        match fs::remove_file(marker) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        // A separate grant refresh also handles bind finishing after the relay
        // has already tried to authenticate this conversation.
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join(REFRESH))?
            .set_modified(SystemTime::now())?;
        println!("Notifications enabled for {}", scope.address());
    }
    Ok(())
}

#[derive(Eq, PartialEq)]
struct Selection {
    scopes: Vec<session::Scope>,
    refresh: Option<SystemTime>,
}

impl Selection {
    fn read(projects: &[PathBuf]) -> io::Result<Self> {
        let directory = relay_directory()?;
        let refresh = match fs::metadata(directory.join(REFRESH)) {
            Ok(metadata) => Some(metadata.modified()?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let scopes =
            session::watch_scopes(projects, |uuid| pause_file(&directory, uuid).try_exists())?;
        Ok(Self { scopes, refresh })
    }
}

fn lock() -> io::Result<File> {
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open("notify.lock")?;
    // SAFETY: the fd belongs to the open file, held for the entire watch.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Err(invalid(
                "a notification watch is already running on this Mac",
            ));
        }
        return Err(error);
    }
    Ok(lock)
}

fn read_line(reader: &mut impl BufRead, limit: u64) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    reader.take(limit + 1).read_until(b'\n', &mut line)?;
    if !line.is_empty() && (!line.ends_with(b"\n") || line.len() as u64 > limit) {
        return Err(invalid("invalid notification stream frame"));
    }
    Ok(line)
}

pub(super) fn sign(key: &std::path::Path, challenge: &[u8], namespace: &str) -> io::Result<String> {
    if challenge.len() != 65
        || challenge[64] != b'\n'
        || !challenge[..64].iter().all(u8::is_ascii_hexdigit)
    {
        return Err(invalid("invalid notification challenge"));
    }
    let mut signer = Command::new("ssh-keygen")
        .args(["-Y", "sign", "-n", namespace, "-f"])
        .arg(key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    signer
        .stdin
        .take()
        .expect("signer stdin is piped")
        .write_all(challenge)?;
    let signed = signer.wait_with_output()?;
    if !signed.status.success() {
        return Err(invalid("could not sign notification challenge"));
    }
    String::from_utf8(signed.stdout).map_err(|_| invalid("invalid notification signature"))
}

pub(super) fn serve_gateway() -> io::Result<()> {
    let executable = env::current_exe()?;
    let home = executable
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or_else(|| invalid("gateway must be installed in its own directory"))?;
    let socket = UnixStream::connect(home.join(".ssh/agentmail-notify.sock"))?;
    socket.set_read_timeout(Some(Duration::from_secs(12)))?;
    socket.set_write_timeout(Some(Duration::from_secs(4)))?;
    let mut reader = BufReader::new(socket);
    let mut output = io::stdout().lock();
    output.write_all(&read_line(&mut reader, 65)?)?;
    output.flush()?;

    let request = read_line(&mut io::stdin().lock(), MAX_AUTH)?;
    reader.get_mut().write_all(&request)?;
    reader.get_mut().shutdown(Shutdown::Write)?;
    loop {
        let line = read_line(&mut reader, MAX)?;
        if line.is_empty() {
            return Ok(());
        }
        output.write_all(&line)?;
        output.flush()?;
    }
}

fn receive(
    ssh: &mut Command,
    latest: &Latest,
    selection: &Selection,
    projects: &[PathBuf],
    ready: &mut bool,
) -> io::Result<Option<Selection>> {
    let mut process = ssh.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let result = (|| {
        let mut stream = BufReader::new(process.stdout.take().expect("SSH stdout is piped"));
        let challenge = read_line(&mut stream, 65)?;
        if challenge.is_empty() {
            return Err(invalid("no response from notification gateway"));
        }
        let mut proofs = Vec::new();
        for scope in &selection.scopes {
            if let Ok(signature) = sign(&scope.key, &challenge, NAMESPACE) {
                proofs.push(serde_json::json!({"scope": scope.binding, "signature": signature}));
            }
        }
        if proofs.is_empty() {
            return Err(invalid("no notification conversation keys could sign"));
        }
        {
            let mut input = process.stdin.take().expect("SSH stdin is piped");
            serde_json::to_writer(&mut input, &proofs)?;
            input.write_all(b"\n")?;
        }
        let mut last_check = Instant::now();
        loop {
            let line = read_line(&mut stream, MAX)?;
            if line.is_empty() {
                return Ok(None);
            }
            if last_check.elapsed() >= Duration::from_secs(1) {
                match Selection::read(projects) {
                    Ok(next) if &next != selection => return Ok(Some(next)),
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!("agentmail watch: could not refresh selection: {error}")
                    }
                }
                last_check = Instant::now();
            }
            let frames: Vec<ScopedSnapshot> = serde_json::from_slice(&line)?;
            let mut snapshots = BTreeMap::new();
            for ScopedSnapshot { scope, snapshot } in frames {
                if !selection
                    .scopes
                    .iter()
                    .any(|known| known.binding == scope && known.address() == snapshot.identity)
                {
                    return Err(invalid(
                        "notification response does not match a requested conversation",
                    ));
                }
                snapshots.insert(scope, snapshot);
            }
            if !snapshots.is_empty() && !*ready {
                println!(
                    "READY: watching {} conversation(s) over one SSH connection",
                    snapshots.len()
                );
                *ready = true;
            }
            *latest.lock().expect("snapshot lock") = Some((Instant::now(), snapshots));
        }
    })();
    *latest.lock().expect("snapshot lock") = None;
    let _ = process.kill();
    let _ = process.wait();
    result
}

fn reconnect(mut ssh: Command, latest: Latest, projects: Vec<PathBuf>, mut selection: Selection) {
    let mut retry = Duration::from_secs(3);
    loop {
        let mut ready = false;
        if selection.scopes.is_empty() {
            eprintln!("agentmail watch: waiting for an enabled Claude conversation");
            while selection.scopes.is_empty() {
                thread::sleep(Duration::from_secs(2));
                match Selection::read(&projects) {
                    Ok(next) => selection = next,
                    Err(error) => {
                        eprintln!("agentmail watch: could not refresh selection: {error}")
                    }
                }
            }
        }
        match receive(&mut ssh, &latest, &selection, &projects, &mut ready) {
            Ok(Some(next)) => {
                selection = next;
                retry = Duration::from_secs(3);
                continue;
            }
            Ok(None) => {}
            Err(error) => eprintln!("agentmail watch: {error}"),
        }
        if ready {
            retry = Duration::from_secs(3);
        }
        eprintln!(
            "agentmail watch: disconnected; retrying in {}s",
            retry.as_secs()
        );
        thread::sleep(retry);
        if let Ok(next) = Selection::read(&projects) {
            selection = next;
        }
        retry = (retry * 2).min(Duration::from_secs(60));
    }
}

pub(super) fn watch(ssh: Command, projects: Vec<PathBuf>) -> io::Result<()> {
    let selection = Selection::read(&projects)?;
    // Bind relative to the private directory to stay within macOS's socket path limit.
    let original_directory = env::current_dir()?;
    let directory = relay_directory()?;
    DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&directory)?;
    env::set_current_dir(directory)?;
    let _lock = lock()?;
    match fs::symlink_metadata(SOCKET) {
        Ok(info) if info.file_type().is_socket() => fs::remove_file(SOCKET)?,
        Ok(_) => return Err(invalid("notification socket path is already occupied")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(SOCKET)?;
    fs::set_permissions(SOCKET, Permissions::from_mode(0o600))?;
    env::set_current_dir(original_directory)?;
    let latest: Latest = Arc::new(Mutex::new(None));
    let reader_state = Arc::clone(&latest);
    thread::spawn(move || reconnect(ssh, reader_state, projects, selection));
    for stream in listener.incoming() {
        let mut stream = stream?;
        let _ = serve_local(&mut stream, &latest);
    }
    Ok(())
}

fn serve_local(stream: &mut UnixStream, latest: &Latest) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    stream.set_write_timeout(Some(Duration::from_millis(250)))?;
    let request = read_line(&mut BufReader::new(&mut *stream), 1024)?;
    let scope: session::Binding = serde_json::from_slice(&request)?;
    let snapshot = latest.lock().expect("snapshot lock");
    let current = snapshot
        .as_ref()
        .filter(|(time, _)| time.elapsed() <= MAX_AGE)
        .and_then(|(_, snapshots)| snapshots.get(&scope));
    serde_json::to_writer(stream, &current)?;
    Ok(())
}

pub(super) fn local_snapshot(args: Vec<String>) -> io::Result<()> {
    let [tool, uuid] = args.as_slice() else {
        return Err(invalid("expected a notification tool and session ID"));
    };
    let Some(scope) = session::configured_for(tool, uuid)? else {
        return Ok(());
    };
    let directory = relay_directory()?;
    if pause_file(&directory, &scope.binding.uuid).try_exists()? {
        return Ok(());
    }
    match env::set_current_dir(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let mut stream = match UnixStream::connect(SOCKET) {
        Ok(socket) => socket,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    stream.set_write_timeout(Some(Duration::from_millis(250)))?;
    serde_json::to_writer(&mut stream, &scope.binding)?;
    stream.write_all(b"\n")?;
    stream.shutdown(Shutdown::Write)?;
    let mut bytes = Vec::new();
    stream.take(MAX + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX {
        return Err(invalid("local notification frame exceeds size limit"));
    }
    if let Some(snapshot) = serde_json::from_slice::<Option<Snapshot>>(&bytes)? {
        if snapshot.identity != scope.address() {
            return Err(invalid("local notification mailbox mismatch"));
        }
        println!("{}", serde_json::to_string(&snapshot)?);
    }
    Ok(())
}
