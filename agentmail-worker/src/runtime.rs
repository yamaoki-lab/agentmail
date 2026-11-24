use crate::{imap::invalid, valid_uuid};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn valid_identity(name: &str) -> bool {
    name.len() <= 32
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub fn valid_blob(value: &str) -> bool {
    // Base64 prefix of the SSH wire encoding of "ssh-ed25519" and key length 32.
    value.len() == 68
        && value.starts_with("AAAAC3NzaC1lZDI1NTE5AAAAI")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
        && matches!(value.as_bytes()[25], b'A'..=b'P')
}

pub fn run(command: &mut Command) -> io::Result<()> {
    if command.status()?.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{:?} failed",
            command.get_program()
        )))
    }
}

pub fn output(command: &mut Command) -> io::Result<String> {
    let result = command.output()?;
    if !result.status.success() {
        return Err(io::Error::other(format!(
            "{:?} failed: {}",
            command.get_program(),
            String::from_utf8_lossy(&result.stderr).trim()
        )));
    }
    String::from_utf8(result.stdout).map_err(|_| invalid("command returned non-UTF-8 output"))
}

pub fn random_hex(bytes: usize) -> io::Result<String> {
    let mut random = vec![0; bytes];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    pub fn new(parent: &Path, prefix: &str) -> io::Result<Self> {
        for _ in 0..8 {
            let path = parent.join(format!("{prefix}{}", random_hex(8)?));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other("could not create a temporary directory"))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn atomic_write(path: &Path, data: &[u8], mode: u32, group: Option<u32>) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("file has no parent directory"))?;
    let stage = TemporaryDirectory::new(parent, ".agentmail-write.")?;
    let temporary = stage.path().join("replacement");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    if let Some(group) = group {
        // SAFETY: the descriptor remains open and group is the configured Unix GID.
        if unsafe { libc::fchown(file.as_raw_fd(), !0, group) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    #[cfg(target_os = "linux")]
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn unix_lock(path: &Path, flags: libc::c_int) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    // SAFETY: the descriptor remains open for the lifetime of the lock.
    if unsafe { libc::flock(file.as_raw_fd(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

pub struct User {
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

pub fn user(name: &str) -> io::Result<User> {
    let name = CString::new(name).map_err(|_| invalid("invalid user name"))?;
    // SAFETY: account lookups run before threads are started and fields are copied immediately.
    unsafe {
        let record = libc::getpwnam(name.as_ptr())
            .as_ref()
            .ok_or_else(|| invalid("Unix user not found"))?;
        Ok(User {
            uid: record.pw_uid,
            gid: record.pw_gid,
            home: PathBuf::from(std::ffi::OsStr::from_bytes(
                CStr::from_ptr(record.pw_dir).to_bytes(),
            )),
        })
    }
}

pub fn group(name: &str) -> io::Result<u32> {
    let name = CString::new(name).map_err(|_| invalid("invalid group name"))?;
    // SAFETY: this is only used by the single-threaded mail container setup.
    unsafe {
        libc::getgrnam(name.as_ptr())
            .as_ref()
            .map(|group| group.gr_gid)
    }
    .ok_or_else(|| invalid("Unix group not found"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub identity: String,
    pub tool: String,
    pub uuid: String,
}

impl Scope {
    pub fn valid(&self) -> bool {
        valid_identity(&self.identity)
            && matches!(self.tool.as_str(), "codex" | "claude")
            && valid_uuid(&self.uuid)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Assignment {
    pub scope: Scope,
    pub blob: String,
}

impl Assignment {
    pub fn authorized_key(&self, gateway: &Path) -> String {
        let Scope {
            identity,
            tool,
            uuid,
        } = &self.scope;
        let blob = &self.blob;
        format!(
            r#"restrict,command="{} --serve-session {identity} {tool} {uuid} {blob}" ssh-ed25519 {blob} agentmail-session"#,
            gateway.display()
        )
    }
}

pub fn registrations(
    lines: &[&str],
    gateway: &Path,
) -> io::Result<BTreeMap<String, (usize, Assignment)>> {
    let mut result = BTreeMap::new();
    let prefix = format!("restrict,command=\"{} --serve-session ", gateway.display());
    for (index, line) in lines.iter().enumerate() {
        let Some(managed) = line.strip_prefix(&prefix) else {
            continue;
        };
        let fields: Vec<_> = managed.split_whitespace().collect();
        let [identity, tool, uuid, blob, _, _, _] = fields.as_slice() else {
            return Err(invalid("invalid managed authorized_keys entry"));
        };
        let entry = Assignment {
            scope: Scope {
                identity: identity.to_string(),
                tool: tool.to_string(),
                uuid: uuid.to_string(),
            },
            blob: blob.trim_end_matches('"').to_owned(),
        };
        if !entry.scope.valid()
            || !valid_blob(&entry.blob)
            || entry.authorized_key(gateway) != *line
            || result
                .insert(entry.scope.identity.clone(), (index, entry))
                .is_some()
        {
            return Err(invalid(
                "invalid or duplicate managed authorized_keys entry",
            ));
        }
    }
    Ok(result)
}
