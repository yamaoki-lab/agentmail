use agentmail_worker::imap::invalid;
use agentmail_worker::runtime::{random_hex, registrations, Assignment, Scope};
use agentmail_worker::Snapshot;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const HEARTBEAT: Duration = Duration::from_secs(4);
type Snapshots = BTreeMap<String, Arc<Snapshot>>;

#[derive(Default)]
pub(super) struct Cache {
    // None marks a stopped worker and invalidates all cached data.
    pub latest: Mutex<Option<Snapshots>>,
    pub changed: Condvar,
}

impl Cache {
    pub(super) fn consume(&self, input: impl Read) -> io::Result<()> {
        let mut input = BufReader::new(input);
        loop {
            let mut line = Vec::new();
            (&mut input)
                .take(128 * 1024 + 1)
                .read_until(b'\n', &mut line)?;
            if line.is_empty() {
                return Ok(());
            }
            if line.len() > 128 * 1024 || !line.ends_with(b"\n") {
                return Err(invalid("invalid IMAP worker snapshot frame"));
            }
            let snapshot: Snapshot = serde_json::from_slice(&line)?;
            self.latest
                .lock()
                .unwrap()
                .get_or_insert_with(BTreeMap::new)
                .insert(snapshot.identity.clone(), Arc::new(snapshot));
            self.changed.notify_all();
        }
    }
}

pub(super) struct Registry {
    pub keys: PathBuf,
    pub gateway: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    scope: Scope,
    signature: String,
}

#[derive(Serialize)]
struct Frame<'a> {
    scope: &'a Scope,
    snapshot: &'a Snapshot,
}

fn peer_uid(socket: &UnixStream) -> io::Result<u32> {
    let mut info = MaybeUninit::<libc::ucred>::uninit();
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the buffer is sized for the Linux SO_PEERCRED result.
    if unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            info.as_mut_ptr().cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if size as usize != std::mem::size_of::<libc::ucred>() {
        return Err(invalid("unexpected Unix socket credentials"));
    }
    // SAFETY: getsockopt succeeded and filled the entire address.
    Ok(unsafe { info.assume_init() }.uid)
}

fn memfile(data: &[u8]) -> io::Result<File> {
    // SAFETY: the kernel copies this constant string, and the returned fd is owned.
    let fd = unsafe { libc::memfd_create(c"agentmail-ssh-proof".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd was just returned uniquely by memfd_create.
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(data)?;
    file.rewind()?;
    // Keep both sources above fd 4, as the verifier receives copies at fds 3 and 4.
    // SAFETY: file remains open during fcntl and the new descriptor is owned.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 5) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(copy) })
}

fn verify(blob: &str, challenge: &[u8], signature: &str, namespace: &str) -> io::Result<bool> {
    if !signature.is_ascii() {
        return Ok(false);
    }
    let signers = memfile(format!("agentmail-hook ssh-ed25519 {blob}\n").as_bytes())?;
    let signed = memfile(signature.as_bytes())?;
    let (signers_fd, signed_fd) = (signers.as_raw_fd(), signed.as_raw_fd());
    let mut command = Command::new("/usr/bin/ssh-keygen");
    command
        .args([
            "-Y",
            "verify",
            "-I",
            "agentmail-hook",
            "-n",
            namespace,
            "-f",
            "/proc/self/fd/3",
            "-s",
            "/proc/self/fd/4",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: these descriptors remain open until spawn returns. dup2 is async-signal-safe,
    // and the inherited copies are created only in the child.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(signers_fd, 3) < 0 || libc::dup2(signed_fd, 4) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut process = command.spawn()?;
    let result = (|| {
        process.stdin.take().unwrap().write_all(challenge)?;
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            if let Some(status) = process.try_wait()? {
                return Ok(status.success());
            }
            thread::sleep(Duration::from_millis(5));
        }
        Ok(false)
    })();
    let _ = process.kill();
    let _ = process.wait();
    result
}

fn challenge(connection: &mut UnixStream) -> io::Result<Vec<u8>> {
    let challenge = (random_hex(32)? + "\n").into_bytes();
    connection.write_all(&challenge)?;
    Ok(challenge)
}

fn authorized(registry: &Registry, assignment: &Assignment) -> io::Result<bool> {
    let entry = assignment.authorized_key(&registry.gateway);
    Ok(fs::read_to_string(&registry.keys)?
        .lines()
        .any(|line| line == entry))
}

fn grant(
    proof: &Proof,
    entries: &BTreeMap<String, (usize, Assignment)>,
    challenge: &[u8],
    tool: &str,
    namespace: &str,
) -> io::Result<Option<Assignment>> {
    if proof.scope.tool != tool || !proof.scope.valid() {
        return Ok(None);
    }
    let Some((_, record)) = entries.get(&proof.scope.identity) else {
        return Ok(None);
    };
    if record.scope != proof.scope || !verify(&record.blob, challenge, &proof.signature, namespace)?
    {
        return Ok(None);
    }
    Ok(Some(record.clone()))
}

pub(super) fn reply(
    mut socket: UnixStream,
    uid: u32,
    cache: &Cache,
    registry: &Registry,
) -> io::Result<()> {
    if peer_uid(&socket)? != uid {
        return Ok(());
    }
    socket.set_read_timeout(Some(Duration::from_millis(400)))?;
    socket.set_write_timeout(Some(Duration::from_millis(400)))?;
    let challenge = challenge(&mut socket)?;
    let mut raw = Vec::new();
    (&mut socket).take(4097).read_to_end(&mut raw)?;
    if raw.len() > 4096 {
        return Err(invalid("oversized notification proof"));
    }
    let proof = serde_json::from_slice(&raw)?;
    let contents = fs::read_to_string(&registry.keys)?;
    let entries = registrations(&contents.lines().collect::<Vec<_>>(), &registry.gateway)?;
    let mut snapshot = None;
    if let Some(assignment) = grant(&proof, &entries, &challenge, "codex", "agentmail-notify-v1")? {
        if authorized(registry, &assignment)? {
            let address = format!("{}@agentmail.test", assignment.scope.identity);
            snapshot = cache
                .latest
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|data| data.get(&address).cloned());
        }
    }
    serde_json::to_writer(&mut socket, &snapshot.as_deref())?;
    socket.write_all(b"\n")
}

pub(super) fn stream(mut socket: UnixStream, cache: &Cache, registry: &Registry) -> io::Result<()> {
    socket.set_read_timeout(Some(Duration::from_secs(12)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let challenge = challenge(&mut socket)?;
    let mut raw = Vec::new();
    BufReader::new(&mut socket)
        .take(16 * 1024 + 1)
        .read_until(b'\n', &mut raw)?;
    if raw.len() > 16 * 1024 || !raw.ends_with(b"\n") {
        return Err(invalid("invalid watch proof frame"));
    }
    let proofs: Vec<Proof> = serde_json::from_slice(&raw)?;
    if proofs.is_empty() || proofs.len() > 16 {
        return Err(invalid("invalid number of watch proofs"));
    }
    let contents = fs::read_to_string(&registry.keys)?;
    let entries = registrations(&contents.lines().collect::<Vec<_>>(), &registry.gateway)?;
    let mut grants = BTreeMap::new();
    for proof in proofs {
        if grants.contains_key(&format!("{}@agentmail.test", proof.scope.identity)) {
            continue;
        }
        if let Some(assignment) =
            grant(&proof, &entries, &challenge, "claude", "agentmail-watch-v1")?
        {
            grants.insert(
                format!("{}@agentmail.test", assignment.scope.identity),
                assignment,
            );
        }
    }
    while !grants.is_empty() {
        let contents = fs::read_to_string(&registry.keys)?;
        grants.retain(|_, grant| {
            let expected = grant.authorized_key(&registry.gateway);
            contents.lines().any(|line| line == expected)
        });
        if grants.is_empty() {
            break;
        }
        let current = {
            let latest = cache.latest.lock().unwrap();
            let Some(latest) = latest.as_ref() else {
                break;
            };
            grants
                .keys()
                .map(|address| (address, latest.get(address).cloned()))
                .collect::<Vec<_>>()
        };
        let frames = current
            .iter()
            .filter_map(|(address, snapshot)| {
                snapshot.as_deref().map(|snapshot| Frame {
                    scope: &grants[*address].scope,
                    snapshot,
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_writer(&mut socket, &frames)?;
        socket.write_all(b"\n")?;
        let _ = cache
            .changed
            .wait_timeout_while(cache.latest.lock().unwrap(), HEARTBEAT, |latest| {
                latest.as_ref().is_some_and(|latest| {
                    current
                        .iter()
                        .all(|(address, prior)| match (prior, latest.get(*address)) {
                            (Some(prior), Some(current)) => Arc::ptr_eq(prior, current),
                            (None, None) => true,
                            _ => false,
                        })
                })
            })
            .unwrap();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentmail_worker::runtime::{run, TemporaryDirectory};
    use std::net::Shutdown;

    fn signed(socket: &mut UnixStream, key: &std::path::Path, namespace: &str) -> String {
        let mut challenge = [0_u8; 65];
        socket.read_exact(&mut challenge).unwrap();
        let mut signer = Command::new("/usr/bin/ssh-keygen")
            .args(["-Y", "sign", "-n", namespace, "-f"])
            .arg(key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        signer.stdin.take().unwrap().write_all(&challenge).unwrap();
        let result = signer.wait_with_output().unwrap();
        assert!(result.status.success());
        String::from_utf8(result.stdout).unwrap()
    }

    #[test]
    fn authenticated_snapshot_scope_and_stream_revocation() {
        let directory =
            TemporaryDirectory::new(&std::env::temp_dir(), "agentmail-native-probe.").unwrap();
        let path = directory.path();
        let gateway = path.join("agentmail-ssh");
        let keys = path.join("authorized_keys");
        let key = path.join("test-key");
        run(Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key))
        .unwrap();
        let public = fs::read_to_string(key.with_extension("pub")).unwrap();
        let blob = public.split_whitespace().nth(1).unwrap().to_owned();
        let registry = Registry { keys, gateway };
        let cache = Cache::default();
        let snapshot = Snapshot {
            identity: "probe@agentmail.test".into(),
            uid_validity: 3,
            unread_count: 1,
            messages: vec![agentmail_worker::Metadata {
                uid: 4,
                sender: "beta@agentmail.test".into(),
                subject: "First".into(),
            }],
        };
        let mut frame = serde_json::to_vec(&snapshot).unwrap();
        frame.push(b'\n');
        cache.consume(frame.as_slice()).unwrap();
        let uuid = "11111111-1111-1111-1111-111111111111";
        let mut scope = Scope {
            identity: "probe".into(),
            tool: "codex".into(),
            uuid: uuid.into(),
        };
        let set = |scope: &Scope| {
            let record = Assignment {
                scope: scope.clone(),
                blob: blob.clone(),
            };
            fs::write(
                &registry.keys,
                record.authorized_key(&registry.gateway) + "\n",
            )
            .unwrap();
        };
        set(&scope);
        for (namespace, succeeds) in [("agentmail-notify-v1", true), ("agentmail-watch-v1", false)]
        {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            thread::scope(|thread| {
                thread.spawn(|| {
                    reply(server, unsafe { libc::geteuid() }, &cache, &registry).unwrap()
                });
                let signature = signed(&mut client, &key, namespace);
                serde_json::to_writer(
                    &mut client,
                    &serde_json::json!({"scope": scope, "signature": signature}),
                )
                .unwrap();
                client.shutdown(Shutdown::Write).unwrap();
                let returned: Option<Snapshot> = serde_json::from_reader(client).unwrap();
                assert_eq!(returned.as_ref(), succeeds.then_some(&snapshot));
            });
        }
        scope.tool = "claude".into();
        set(&scope);
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(6)))
            .unwrap();
        thread::scope(|thread| {
            thread.spawn(|| stream(server, &cache, &registry).unwrap());
            let signature = signed(&mut client, &key, "agentmail-watch-v1");
            serde_json::to_writer(
                &mut client,
                &serde_json::json!([{"scope": scope, "signature": signature}]),
            )
            .unwrap();
            client.write_all(b"\n").unwrap();
            let mut reader = BufReader::new(client);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(frame[0]["snapshot"]["messages"][0]["subject"], "First");
            fs::write(&registry.keys, "").unwrap();
            line.clear();
            assert_eq!(reader.read_line(&mut line).unwrap(), 0);
        });
    }
}
