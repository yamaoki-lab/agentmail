use super::{decode, encode, invalid, mail, Client, Request, PREFIX, SESSION_PREFIX};
use agentmail_worker::{valid_uuid, DATA_DIRECTORY};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

const LEGACY_SESSIONS: &str = ".agentmail/sessions";
const PREPARE_HINT: &str = "session is not configured; run agentmail prepare-session MAILBOX";
const ADMIN: &str = "/home/agentmail/agentmail-ssh-lab/agentmail-admin";
const MAILBOX_MANAGER: &str = "/home/agentmail/agentmail-mailboxes-lab/manage-vm-mailboxes.sh";

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    identity: String,
    pub(super) tool: String,
    pub(super) uuid: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionRequest {
    pub(super) scope: Binding,
    pub(super) request: Request,
}

#[derive(Eq, PartialEq)]
pub(super) struct Scope {
    pub(super) binding: Binding,
    pub(super) key: PathBuf,
}

fn valid_identity(value: &str) -> bool {
    value.len() <= 32
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_blob(value: &str) -> bool {
    value.len() == 68
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
}

fn conversation() -> io::Result<Option<(String, String)>> {
    let codex = env::var("CODEX_THREAD_ID").ok().filter(|id| !id.is_empty());
    let claude = env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .filter(|id| !id.is_empty());
    let (tool, uuid) = match (codex, claude) {
        (Some(id), None) => ("codex", id),
        (None, Some(id)) => ("claude", id),
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => return Err(invalid("both Codex and Claude session IDs are set")),
    };
    let uuid = uuid.to_ascii_lowercase();
    if !valid_uuid(&uuid) {
        return Err(invalid("invalid conversation UUID"));
    }
    Ok(Some((tool.to_owned(), uuid)))
}

fn repo_root(directory: &Path) -> io::Result<Option<PathBuf>> {
    let output = match Command::new("git")
        .current_dir(directory)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let path =
        String::from_utf8(output.stdout).map_err(|_| invalid("Git root path is not UTF-8"))?;
    Ok(Some(PathBuf::from(
        path.strip_suffix('\n').unwrap_or(&path),
    )))
}

fn data_dir() -> io::Result<PathBuf> {
    let home = env::var_os("HOME").ok_or_else(|| invalid("HOME is not set"))?;
    Ok(PathBuf::from(home).join(DATA_DIRECTORY))
}

fn read_identity(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content.trim_end_matches('\n').to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn legacy_identity(root: &Path, name: &str) -> io::Result<Option<String>> {
    let path = Path::new(LEGACY_SESSIONS).join(name);
    let Some(identity) = read_identity(&root.join(&path))? else {
        return Ok(None);
    };
    let ignored = Command::new("git")
        .current_dir(root)
        .args(["check-ignore", "-q", "--"])
        .arg(path)
        .status()?;
    if !ignored.success() {
        return Err(invalid("legacy session configuration must be Git-ignored"));
    }
    Ok(Some(identity))
}

fn configured_identity(directory: &Path, name: &str) -> io::Result<Option<String>> {
    if let Some(identity) = read_identity(&directory.join(name))? {
        return Ok(Some(identity));
    }
    match repo_root(Path::new("."))? {
        Some(root) => legacy_identity(&root, name),
        None => Ok(None),
    }
}

fn scope(identity: String, tool: String, uuid: String) -> io::Result<Scope> {
    if !valid_identity(&identity) {
        return Err(invalid("invalid short mailbox name"));
    }
    let home = PathBuf::from(env::var_os("HOME").ok_or_else(|| invalid("HOME is not set"))?);
    let session_dir = format!("{tool}-{uuid}");
    let key_dir = data_dir()?.join("keys").join(&session_dir);
    let legacy_key = home
        .join(".ssh/agentmail-sessions")
        .join(&session_dir)
        .join("id_ed25519");
    let key = if !key_dir.try_exists()? && legacy_key.try_exists()? {
        legacy_key
    } else {
        key_dir.join("id_ed25519")
    };
    Ok(Scope {
        binding: Binding {
            identity,
            tool,
            uuid,
        },
        key,
    })
}

pub(super) fn configured() -> io::Result<Option<Scope>> {
    let Some((tool, uuid)) = conversation()? else {
        return Ok(None);
    };
    configured_for(&tool, &uuid)?
        .map(Some)
        .ok_or_else(|| invalid(PREPARE_HINT))
}

pub(super) fn configured_for(tool: &str, uuid: &str) -> io::Result<Option<Scope>> {
    let uuid = uuid.to_ascii_lowercase();
    if !matches!(tool, "codex" | "claude") || !valid_uuid(&uuid) {
        return Err(invalid("invalid notification conversation"));
    }
    let name = format!("{tool}-{uuid}");
    let Some(identity) = configured_identity(&data_dir()?.join("sessions"), &name)? else {
        return Ok(None);
    };
    let scope = scope(identity, tool.to_owned(), uuid)?;
    if !scope.key.try_exists()? {
        return Err(invalid(
            "session key is missing; run agentmail prepare-session MAILBOX",
        ));
    }
    Ok(Some(scope))
}

fn claude_registrations(directory: &Path) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut uuids = Vec::new();
    for entry in entries {
        if let Some(uuid) = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix("claude-"))
            .filter(|uuid| valid_uuid(uuid))
        {
            uuids.push(uuid.to_owned());
        }
    }
    Ok(uuids)
}

pub(super) fn watch_scopes(
    projects: &[PathBuf],
    is_paused: impl Fn(&str) -> io::Result<bool>,
) -> io::Result<Vec<Scope>> {
    let directory = data_dir()?.join("sessions");
    let mut registrations = std::collections::BTreeMap::new();
    for uuid in claude_registrations(&directory)? {
        if let Some(identity) = read_identity(&directory.join(format!("claude-{uuid}")))? {
            registrations.insert(uuid, identity);
        }
    }
    for project in projects {
        let root = repo_root(project)?
            .ok_or_else(|| invalid("legacy notification project must be a Git working tree"))?;
        for uuid in claude_registrations(&root.join(LEGACY_SESSIONS))? {
            if let Some(identity) = legacy_identity(&root, &format!("claude-{uuid}"))? {
                match registrations.entry(uuid) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(identity);
                    }
                    std::collections::btree_map::Entry::Occupied(entry)
                        if entry.get() != &identity =>
                    {
                        return Err(invalid(
                            "conflicting mailbox names for a Claude conversation",
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
    let mut scopes = Vec::new();
    for (uuid, identity) in registrations {
        if is_paused(&uuid)? {
            continue;
        }
        let scope = scope(identity, "claude".to_owned(), uuid)?;
        if scope.key.try_exists()? {
            scopes.push(scope);
        }
    }
    if scopes.len() > 16 {
        return Err(invalid(
            "watch supports up to 16 unpaused Claude conversations",
        ));
    }
    Ok(scopes)
}

pub(super) fn notification_snapshot(args: Vec<String>) -> io::Result<()> {
    let [socket, tool, uuid] = args.as_slice() else {
        return Err(invalid("expected a socket, conversation tool and UUID"));
    };
    let Some(scope) = configured_for(tool, uuid)? else {
        return Ok(());
    };
    let mut socket = UnixStream::connect(socket)?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
    let mut challenge = Vec::new();
    BufReader::new((&mut socket).take(66)).read_until(b'\n', &mut challenge)?;
    let signature = super::notify::sign(&scope.key, &challenge, "agentmail-notify-v1")?;
    serde_json::to_writer(
        &mut socket,
        &serde_json::json!({
            "scope": scope.binding,
            "signature": signature,
        }),
    )?;
    socket.shutdown(Shutdown::Write)?;
    let mut response = Vec::new();
    socket.take(128 * 1024 + 1).read_to_end(&mut response)?;
    if response.len() > 128 * 1024 {
        return Err(invalid("notification response exceeds the limit"));
    }
    let data: serde_json::Value = serde_json::from_slice(&response)?;
    if !data.is_null() {
        let identity = format!("{}@agentmail.test", scope.binding.identity);
        if data.get("identity").and_then(serde_json::Value::as_str) != Some(&identity) {
            return Err(invalid("notification response does not match the mailbox"));
        }
        println!("{data}");
    }
    Ok(())
}

fn prepare_scope(identity: &str) -> io::Result<Scope> {
    let (tool, uuid) =
        conversation()?.ok_or_else(|| invalid("run agentmail from the Codex or Claude session"))?;
    let name = format!("{tool}-{uuid}");
    let directory = data_dir()?.join("sessions");
    let path = directory.join(&name);
    let scope = scope(identity.to_owned(), tool, uuid)?;
    let current = configured_identity(&directory, &name)?;
    if current
        .as_deref()
        .is_some_and(|current| current != identity)
    {
        return Err(invalid(
            "another mailbox is configured for this conversation",
        ));
    }

    let key_dir = scope.key.parent().expect("session key has a parent");
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(key_dir.parent().expect("key directory has a parent"))?;
    match fs::DirBuilder::new().mode(0o700).create(key_dir) {
        Ok(()) => {
            let result = Command::new("ssh-keygen")
                .args([
                    "-q",
                    "-t",
                    "ed25519",
                    "-N",
                    "",
                    "-C",
                    "agentmail-session",
                    "-f",
                ])
                .arg(&scope.key)
                .status();
            if !matches!(result, Ok(status) if status.success()) {
                fs::remove_dir_all(key_dir)?;
                return Err(invalid("could not generate the agentmail session key"));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    scope.public_blob()?;
    if read_identity(&path)?.as_deref() == Some(identity) {
        return Ok(scope);
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&directory)?;
    let staging = directory.join(format!(".{name}.{}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)?;
    let publish = (|| {
        writeln!(file, "{identity}")?;
        match fs::hard_link(&staging, &path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if read_identity(&path)?.as_deref() != Some(identity) {
                    return Err(invalid(
                        "another mailbox is configured for this conversation",
                    ));
                }
                Ok(())
            }
            Err(error) => Err(error),
        }
    })();
    let cleanup = fs::remove_file(staging);
    publish?;
    cleanup?;
    Ok(scope)
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum RegistrationStatus {
    Missing,
    Unassigned,
    Registered,
    Assigned { old_tool: String, old_uuid: String },
    ConversationInUse,
}

pub(super) fn prepare(client: &Client, identity: &str, offline: bool) -> io::Result<()> {
    let scope = prepare_scope(identity).inspect_err(|error| {
        if error.kind() == io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(libc::EROFS) {
            eprintln!("ローカルの会話登録を書き込めません。サンドボックス内なら承認付きで prepare-session を再試行してください。");
        }
    })?;
    let blob = scope.public_blob()?;
    if offline {
        println!("VM の状態は未確認です。必要な操作を メールホストの管理用 SSH で選んでください。");
        println!("受信箱が無い場合に実行");
        scope.print_create();
        println!("受信箱が未割り当ての場合に実行");
        scope.print_bind(&blob);
        return Ok(());
    }

    let request = encode(
        PREFIX,
        &Request::SessionStatus {
            scope: scope.binding.clone(),
            blob: blob.clone(),
        },
    )?;
    let output = client
        .ssh(&client.key, false)
        .arg(request)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        io::stderr().write_all(&output.stderr)?;
        return Err(invalid("VM の登録状態を確認できません。サンドボックス内なら承認付きで prepare-session を再試行してください。VM の Agentmail ゲートウェイが旧版の場合は更新が必要です。オフラインでコマンドだけ表示するには --offline を指定してください。"));
    }
    let status: RegistrationStatus = serde_json::from_slice(&output.stdout)?;
    match status {
        RegistrationStatus::Missing => {
            println!(
                "{} はまだありません。メールホストの管理用 SSH で以下を上から実行してください。",
                scope.address()
            );
            scope.print_create();
            scope.print_bind(&blob);
        }
        RegistrationStatus::Unassigned => {
            println!(
                "{} は作成済みで未割り当てです。メールホストの管理用 SSH で以下を実行してください。",
                scope.address()
            );
            scope.print_bind(&blob);
        }
        RegistrationStatus::Assigned { old_tool, old_uuid } => {
            if !matches!(old_tool.as_str(), "codex" | "claude") || !valid_uuid(&old_uuid) {
                return Err(invalid("invalid current mailbox assignment"));
            }
            let Binding {
                identity,
                tool,
                uuid,
            } = &scope.binding;
            println!("{} は別の会話に割り当て済みです。引き継ぐ場合だけ メールホストの管理用 SSH で以下を実行してください。", scope.address());
            println!("sudo -iu agentmail {ADMIN} rebind {identity} {old_tool} {old_uuid} {tool} {uuid} {blob}");
        }
        RegistrationStatus::ConversationInUse => {
            return Err(invalid("この会話 ID には VM に別の割り当てがあるか、公開鍵が異なります。メールホストの管理用 SSH で agentmail-admin list を確認してください。"));
        }
        RegistrationStatus::Registered => {
            let request = encode(
                SESSION_PREFIX,
                &SessionRequest {
                    scope: scope.binding.clone(),
                    request: Request::Whoami {},
                },
            )?;
            let output = client
                .ssh(&scope.key, false)
                .arg(request)
                .stdin(Stdio::null())
                .output()?;
            let expected = format!(
                "{} ({} {}, registered)",
                scope.address(),
                scope.binding.tool,
                scope.binding.uuid
            );
            if !output.status.success()
                || String::from_utf8_lossy(&output.stdout).trim() != expected
            {
                io::stderr().write_all(&output.stderr)?;
                return Err(invalid("VM に同じ割り当てがありますが、専用鍵で whoami を確認できません。サンドボックス内なら承認付きで whoami を再試行してください。"));
            }
            println!("{expected}");
        }
    }
    scope.print_notification_help();
    Ok(())
}

impl Scope {
    pub(super) fn address(&self) -> String {
        format!("{}@agentmail.test", self.binding.identity)
    }

    fn print_create(&self) {
        println!(
            "sudo -iu agentmail sh {MAILBOX_MANAGER} identity create {}",
            self.binding.identity
        );
    }

    fn print_bind(&self, blob: &str) {
        let Binding {
            identity,
            tool,
            uuid,
        } = &self.binding;
        println!("sudo -iu agentmail {ADMIN} bind {identity} {tool} {uuid} {blob}");
    }

    fn print_notification_help(&self) {
        match self.binding.tool.as_str() {
            "claude" if cfg!(target_os = "macos") => {
                println!(concat!(
                    "Mac の通知監視を始める場合は、Mac の通常のターミナルで ",
                    "agentmail watch-notifications を実行してください。",
                    "既に監視中なら、VM での登録後にこの会話で ",
                    "agentmail watch-notifications --resume を実行してください。"
                ));
            }
            "codex" => {
                println!(concat!(
                    "VM の通知中継が停止中の場合は、VM の管理用 SSH で ",
                    "sudo systemctl start agentmail-notify.service を実行してください。"
                ));
            }
            _ => {}
        }
    }

    fn public_blob(&self) -> io::Result<String> {
        let public_key = fs::read_to_string(self.key.with_extension("pub"))?;
        let mut fields = public_key.split_whitespace();
        let blob = fields
            .next()
            .filter(|algorithm| *algorithm == "ssh-ed25519")
            .and_then(|_| fields.next())
            .filter(|blob| valid_blob(blob))
            .ok_or_else(|| invalid("invalid session public key"))?;
        Ok(blob.to_owned())
    }
}

pub(super) fn serve_status(scope: Binding, blob: &str) -> io::Result<ExitStatus> {
    if !valid_identity(&scope.identity)
        || !matches!(scope.tool.as_str(), "codex" | "claude")
        || !valid_uuid(&scope.uuid)
        || !valid_blob(blob)
    {
        return Err(invalid("invalid mailbox registration status request"));
    }
    Command::new(env::current_exe()?.with_file_name("agentmail-admin"))
        .args(["status", &scope.identity, &scope.tool, &scope.uuid, blob])
        .stdin(Stdio::null())
        .status()
}

pub(super) fn serve(args: Vec<String>) -> Result<Option<ExitStatus>, Box<dyn std::error::Error>> {
    let [identity, tool, uuid, blob] = args.as_slice() else {
        return Err(invalid("invalid forced session command").into());
    };
    if !valid_identity(identity)
        || !matches!(tool.as_str(), "codex" | "claude")
        || !valid_uuid(uuid)
        || !valid_blob(blob)
    {
        return Err(invalid("invalid forced session command").into());
    }
    let request: SessionRequest = decode(
        SESSION_PREFIX,
        &env::var("SSH_ORIGINAL_COMMAND").unwrap_or_default(),
    )?;
    if request.scope.identity != *identity
        || request.scope.tool != *tool
        || request.scope.uuid != *uuid
    {
        return Err(invalid("conversation or mailbox does not match the SSH assignment").into());
    }

    // Revalidate against the current file so even an existing SSH connection
    // cannot open more mailbox commands after the assignment is revoked.
    let executable = env::current_exe()?;
    let home = executable
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| invalid("gateway must be installed in its own directory"))?;
    let line = format!(
        "restrict,command=\"{} --serve-session {identity} {tool} {uuid} {blob}\" ssh-ed25519 {blob} agentmail-session",
        executable.display()
    );
    let keys = fs::read_to_string(home.join(".ssh/authorized_keys"))?;
    if !keys.lines().any(|entry| entry == line) {
        return Err(invalid("mailbox assignment has been revoked").into());
    }
    if request.request == (Request::Whoami {}) {
        println!("{identity}@agentmail.test ({tool} {uuid}, registered)");
        return Ok(None);
    }
    mail(identity, request.request)
}
