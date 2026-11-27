#[path = "protocol.rs"]
mod protocol;

use self::protocol::{Cache, Registry};
use agentmail_worker::imap::invalid;
use agentmail_worker::runtime::{output, run as checked, unix_lock, user, User};
use std::collections::BTreeMap;
use std::env;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixDatagram, UnixListener};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

const CONTAINER: &str = "agentmail-mail-lab";
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn stopped() -> bool {
    STOP.load(Ordering::Relaxed)
}

fn signal_ready() -> io::Result<()> {
    if let Some(path) = env::var_os("NOTIFY_SOCKET") {
        let socket = UnixDatagram::unbound()?;
        if let Some(name) = path.as_bytes().strip_prefix(b"@") {
            socket.send_to_addr(b"READY=1", &SocketAddr::from_abstract_name(name)?)?;
        } else {
            socket.send_to(b"READY=1", Path::new(&path))?;
        }
    }
    Ok(())
}

fn open_directory(parent: &File, name: &std::ffi::OsStr) -> io::Result<File> {
    let name =
        CString::new(name.as_bytes()).map_err(|_| invalid("invalid socket path component"))?;
    // SAFETY: parent stays open and name is a valid C string.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn fresh_socket(path: &Path) -> io::Result<UnixListener> {
    match fs::symlink_metadata(path) {
        Ok(info) if info.file_type().is_socket() => fs::remove_file(path)?,
        Ok(_) => return Err(invalid("notification socket path is occupied")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    UnixListener::bind(path)
}

fn public_listener(path: &Path, codex: &User) -> io::Result<UnixListener> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("socket directory has no parent"))?;
    if !parent.is_absolute() {
        return Err(invalid("expected an absolute socket directory"));
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open("/")?;
    for component in parent.components().skip(1) {
        let Component::Normal(name) = component else {
            return Err(invalid("invalid socket path"));
        };
        directory = open_directory(&directory, name)?;
    }
    if directory.metadata()?.uid() != codex.uid {
        return Err(invalid(
            "unexpected owner of the shared development directory",
        ));
    }
    let name = path
        .file_name()
        .ok_or_else(|| invalid("no socket directory name"))?;
    let c_name = CString::new(name.as_bytes()).map_err(|_| invalid("invalid directory name"))?;
    // SAFETY: parent is a pinned directory and c_name is a single path component.
    if unsafe { libc::mkdirat(directory.as_raw_fd(), c_name.as_ptr(), 0o700) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    directory = open_directory(&directory, name)?;
    let info = directory.metadata()?;
    match (info.uid(), info.gid(), info.mode() & 0o7777) {
        (0, 0, 0o700) => {
            // SAFETY: the directory is owned by root and was opened without following symlinks.
            if unsafe { libc::fchown(directory.as_raw_fd(), 0, codex.gid) } != 0 {
                return Err(io::Error::last_os_error());
            }
            directory.set_permissions(Permissions::from_mode(0o2710))?;
        }
        (0, gid, 0o2710) if gid == codex.gid => {}
        _ => {
            return Err(invalid(
                "unexpected notification socket directory permissions",
            ))
        }
    }
    // SAFETY: the fd remains open. This runs before any thread is created.
    if unsafe { libc::fchdir(directory.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let path = Path::new("notify.sock");
    let listener = fresh_socket(path)?;
    fs::set_permissions(path, Permissions::from_mode(0o660))?;
    Ok(listener)
}

fn serve(
    public: &UnixListener,
    private: &UnixListener,
    worker: &mut Child,
    uid: u32,
    registry: Arc<Registry>,
) -> io::Result<()> {
    let cache = Arc::new(Cache {
        latest: Some(BTreeMap::new()).into(),
        ..Cache::default()
    });
    let started_worker = worker
        .stdout
        .take()
        .ok_or_else(|| invalid("worker has no stdout"))?;
    let reader_cache = cache.clone();
    let reader = thread::spawn(move || {
        if let Err(error) = reader_cache.consume(started_worker) {
            eprintln!("agentmail-notify: {error}");
        }
        *reader_cache.latest.lock().unwrap() = None;
        reader_cache.changed.notify_all();
    });
    let sessions = Arc::new(AtomicUsize::new(0));
    let mut sockets = [
        libc::pollfd {
            fd: public.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: private.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    signal_ready()?;
    println!("Agentmail notification socket ready.");
    while !stopped() && !reader.is_finished() && worker.try_wait()?.is_none() {
        // SAFETY: the descriptors are borrowed from live listeners and sockets is bounded.
        if unsafe { libc::poll(sockets.as_mut_ptr(), sockets.len() as _, 500) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if sockets[0].revents & libc::POLLIN != 0 {
            let (connection, _) = public.accept()?;
            let _ = protocol::reply(connection, uid, &cache, &registry);
        }
        if sockets[1].revents & libc::POLLIN != 0 {
            let (connection, _) = private.accept()?;
            if sessions
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    (n < 16).then_some(n + 1)
                })
                .is_err()
            {
                continue;
            }
            let (sessions, cache, registry) = (sessions.clone(), cache.clone(), registry.clone());
            thread::spawn(move || {
                let _ = protocol::stream(connection, &cache, &registry);
                sessions.fetch_sub(1, Ordering::Relaxed);
            });
        }
    }
    *cache.latest.lock().unwrap() = None;
    cache.changed.notify_all();
    Ok(())
}

fn podman() -> Command {
    let mut command = Command::new("/usr/bin/podman");
    command.args(["--cgroup-manager=cgroupfs"]);
    command
}

fn supervise(
    public: &UnixListener,
    private: &UnixListener,
    uid: u32,
    registry: Arc<Registry>,
) -> io::Result<()> {
    let directory = output(podman().args([
        "exec",
        CONTAINER,
        "mktemp",
        "-d",
        "/tmp/agentmail-notify.XXXXXXXX",
    ]))?;
    let stage = directory.trim();
    if !stage
        .strip_prefix("/tmp/agentmail-notify.")
        .is_some_and(|suffix| {
            suffix.len() == 8 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
    {
        return Err(invalid("invalid worker staging directory"));
    }
    let result = (|| {
        let worker_binary = env::current_exe()?.with_file_name("agentmail-worker");
        checked(
            podman()
                .arg("cp")
                .arg(worker_binary)
                .arg(format!("{CONTAINER}:{stage}/worker")),
        )?;
        let mut worker = podman().args([
            "exec", CONTAINER, "sh", "-c",
            r#"printf '%s\n' "$$" > "$1/pid"; exec "$1/worker" --stream-reload /run/agentmail-lab/accounts.json"#,
            "sh", stage,
        ]).stdout(Stdio::piped()).spawn()?;
        let result = serve(public, private, &mut worker, uid, registry);
        if worker.try_wait().ok().flatten().is_none() {
            let _ = checked(Command::new("/usr/bin/timeout").args([
                "5", "/usr/bin/podman", "--cgroup-manager=cgroupfs", "exec", CONTAINER, "sh", "-c",
                r#"if read -r pid < "$1/pid" && [ "$(readlink "/proc/$pid/exe")" = "$1/worker" ]; then kill -TERM "$pid"; fi"#,
                "sh", stage,
            ]));
            let _ = worker.kill();
            let _ = worker.wait();
        }
        result
    })();
    let _ = checked(Command::new("/usr/bin/timeout").args([
        "5",
        "/usr/bin/podman",
        "--cgroup-manager=cgroupfs",
        "exec",
        CONTAINER,
        "rm",
        "-rf",
        "--",
        stage,
    ]));
    result
}

pub(super) fn run() -> io::Result<()> {
    if env::args().nth(1).as_deref() == Some("--help") {
        println!("Usage: agentmail-notify [--help]");
        return Ok(());
    }
    if env::args_os().len() > 1 {
        return Err(invalid("unexpected argument"));
    }
    // SAFETY: we are still single threaded; the handler only sets an atomic flag.
    unsafe {
        if libc::geteuid() != 0 {
            return Err(invalid("start under systemd as root"));
        }
        libc::umask(0o077);
        if libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t) == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    let _lock = unix_lock(
        Path::new("/run/agentmail-notify.lock"),
        libc::LOCK_EX | libc::LOCK_NB,
    )?;
    let service = user("agentmail")?;
    let codex = user("codex")?;
    let path = env::var_os("AGENTMAIL_SOCKET_DIRECTORY")
        .map(PathBuf::from)
        .ok_or_else(|| {
            invalid("configure AGENTMAIL_SOCKET_DIRECTORY before starting this service")
        })?;
    let public = public_listener(&path, &codex)?;
    // SAFETY: drop privileges before starting any worker or serving connections.
    if unsafe { libc::setgroups(0, std::ptr::null()) } != 0
        || unsafe { libc::setgid(service.gid) } != 0
        || unsafe { libc::setuid(service.uid) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    env::set_current_dir(&service.home)?;
    env::set_var("HOME", &service.home);
    env::set_var("USER", "agentmail");
    env::set_var("LOGNAME", "agentmail");
    let registry = Arc::new(Registry {
        keys: service.home.join(".ssh/authorized_keys"),
        gateway: service.home.join("agentmail-ssh-lab/agentmail-ssh"),
    });
    let runtime = fs::read_to_string(registry.gateway.with_file_name("runtime-dir"))?;
    if !Path::new(runtime.trim()).is_absolute() || !Path::new(runtime.trim()).is_dir() {
        return Err(invalid("invalid Podman runtime directory"));
    }
    env::set_var("XDG_RUNTIME_DIR", runtime.trim());
    let private_path = service.home.join(".ssh/agentmail-notify.sock");
    let private = fresh_socket(&private_path)?;
    let result = supervise(&public, &private, codex.uid, registry);
    let _ = fs::remove_file(private_path);
    result
}
