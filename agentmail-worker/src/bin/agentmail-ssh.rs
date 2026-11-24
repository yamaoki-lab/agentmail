//! Restricted SSH transport for the isolated mail lab.
#[path = "agentmail-ssh/notify.rs"]
mod notify;
#[path = "agentmail-ssh/session.rs"]
mod session;

use agentmail_worker::upload::{read_limited, valid_filename, write_frame, MAX_BODY, MAX_PATCH};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

const PREFIX: &str = "agentmail-v1 ";
const SESSION_PREFIX: &str = "agentmail-session-v1 ";
const MAX_JSON: usize = 8192;
const MAIL_CONTAINER: &str = "agentmail-mail-lab";
const LAB_CLI: &str = "/opt/agentmail-ssh-lab";

#[derive(Parser)]
#[command(about = "Operate a lab mailbox via a dedicated SSH key")]
struct Client {
    #[arg(long, default_value = "host.containers.internal")]
    host: String,
    #[arg(long, default_value_t = 22)]
    port: u16,
    /// Connect using IPv4 only.
    #[arg(long)]
    ipv4: bool,
    #[arg(long)]
    key: PathBuf,
    #[arg(long)]
    known_hosts: PathBuf,
    #[command(subcommand)]
    request: Request,
}

#[derive(Debug, Subcommand, Serialize, Deserialize, PartialEq)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Whoami {},
    /// Prepare a dedicated key and check whether VM registration is needed.
    PrepareSession {
        identity: String,
        /// Prepare the key and show commands without checking the VM.
        #[arg(long)]
        #[serde(skip)]
        offline: bool,
    },
    #[command(skip)]
    SessionStatus {
        scope: session::Binding,
        blob: String,
    },
    /// Keep one local notification relay open for prepared Claude conversations.
    WatchNotifications {
        /// Also include legacy registrations from this Git working tree. Repeat as needed.
        #[arg(long)]
        #[serde(skip)]
        project: Vec<PathBuf>,
        /// Pause notifications for the current Claude conversation.
        #[arg(long, conflicts_with_all = ["project", "resume"])]
        #[serde(skip)]
        pause: bool,
        /// Enable notifications for the current Claude conversation and refresh its grant.
        #[arg(long, conflicts_with = "project")]
        #[serde(skip)]
        resume: bool,
    },
    Send {
        #[arg(long, required = true)]
        to: Vec<String>,
        #[arg(long)]
        cc: Vec<String>,
        #[arg(long, required_unless_present = "inline_patch")]
        subject: Option<String>,
        #[arg(long)]
        markdown: bool,
        /// Send a single git format-patch message as the mail body.
        #[arg(long, conflicts_with_all = ["subject", "markdown", "patch"])]
        #[serde(skip)]
        inline_patch: Option<PathBuf>,
        /// Attach the original patch bytes to the message.
        #[arg(long)]
        #[serde(skip)]
        patch: Option<PathBuf>,
        #[arg(skip)]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upload: Option<SendUpload>,
    },
    Inbox {
        #[arg(long)]
        unread: bool,
    },
    Read {
        uid: u32,
    },
    Reply {
        uid: u32,
        #[arg(long)]
        markdown: bool,
        /// Attach the original patch bytes to the reply.
        #[arg(long)]
        #[serde(skip)]
        patch: Option<PathBuf>,
        #[arg(skip)]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upload: Option<AttachmentUpload>,
    },
    Export {
        uid: u32,
        #[arg(long)]
        attachment: Option<String>,
    },
    Mark {
        uid: u32,
        state: ReadState,
    },
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SendUpload {
    Inline,
    Attachment { filename: String },
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct AttachmentUpload {
    filename: String,
}

struct UploadPayload {
    patch: Vec<u8>,
    body: Option<Vec<u8>>,
}

fn read_patch(path: &Path) -> io::Result<Vec<u8>> {
    if !fs::metadata(path)?.is_file() {
        return Err(invalid("expected a regular patch file"));
    }
    read_limited(File::open(path)?, MAX_PATCH)
}

fn attachment(path: &Path) -> io::Result<(String, UploadPayload)> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| valid_filename(name))
        .ok_or_else(|| invalid("invalid patch attachment filename"))?;
    let patch = read_patch(path)?;
    let body = read_limited(io::stdin().lock(), MAX_BODY)?;
    std::str::from_utf8(&body).map_err(|_| invalid("mail body must be UTF-8"))?;
    Ok((
        filename.to_owned(),
        UploadPayload {
            patch,
            body: Some(body),
        },
    ))
}

fn prepare_upload(request: &mut Request) -> io::Result<Option<UploadPayload>> {
    match request {
        Request::Send {
            inline_patch: Some(path),
            upload,
            ..
        } => {
            let patch = read_patch(path)?;
            *upload = Some(SendUpload::Inline);
            Ok(Some(UploadPayload { patch, body: None }))
        }
        Request::Send {
            patch: Some(path),
            upload,
            ..
        } => {
            let (filename, payload) = attachment(path)?;
            *upload = Some(SendUpload::Attachment { filename });
            Ok(Some(payload))
        }
        Request::Reply {
            patch: Some(path),
            upload,
            ..
        } => {
            let (filename, payload) = attachment(path)?;
            *upload = Some(AttachmentUpload { filename });
            Ok(Some(payload))
        }
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ReadState {
    Read,
    Unread,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn encode<T: Serialize>(prefix: &str, request: &T) -> io::Result<String> {
    let bytes = serde_json::to_vec(request)?;
    if bytes.len() > MAX_JSON {
        return Err(invalid("request exceeds size limit"));
    }
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok(format!("{prefix}{hex}"))
}

fn decode<T: serde::de::DeserializeOwned>(prefix: &str, command: &str) -> io::Result<T> {
    let raw = command
        .strip_prefix(prefix)
        .ok_or_else(|| invalid("use agentmail-ssh; shell access is disabled"))?;
    if raw.is_empty()
        || raw.len() > MAX_JSON * 2
        || raw.len() % 2 != 0
        || !raw.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid("invalid agentmail request"));
    }
    let mut bytes = Vec::with_capacity(raw.len() / 2);
    for index in (0..raw.len()).step_by(2) {
        bytes.push(
            u8::from_str_radix(&raw[index..index + 2], 16)
                .map_err(|_| invalid("invalid agentmail encoding"))?,
        );
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid("unsupported agentmail request"))
}

fn cli_arguments(request: Request) -> Vec<String> {
    let mut args = Vec::new();
    match request {
        Request::Whoami {}
        | Request::PrepareSession { .. }
        | Request::SessionStatus { .. }
        | Request::WatchNotifications { .. } => {
            unreachable!("handled before the mail CLI is invoked")
        }
        Request::Send {
            to,
            cc,
            subject,
            markdown,
            upload,
            ..
        } => {
            args.push("send".into());
            args.extend(to.into_iter().map(|address| format!("--to={address}")));
            args.extend(cc.into_iter().map(|address| format!("--cc={address}")));
            if let Some(subject) = subject {
                args.push(format!("--subject={subject}"));
            }
            match upload {
                Some(SendUpload::Inline) => args.push("--upload-inline".into()),
                Some(SendUpload::Attachment { filename }) => {
                    args.push(format!("--upload-attachment={filename}"));
                }
                None => {}
            }
            if markdown {
                args.push("--markdown".into());
            }
        }
        Request::Inbox { unread } => {
            args.push("inbox".into());
            if unread {
                args.push("--unread".into());
            }
        }
        Request::Read { uid } => args.extend(["read".into(), uid.to_string()]),
        Request::Reply {
            uid,
            markdown,
            upload,
            ..
        } => {
            args.extend(["reply".into(), uid.to_string()]);
            if markdown {
                args.push("--markdown".into());
            }
            if let Some(upload) = upload {
                args.push(format!("--upload-attachment={}", upload.filename));
            }
        }
        Request::Export { uid, attachment } => {
            args.extend(["export".into(), uid.to_string()]);
            if let Some(name) = attachment {
                args.push(format!("--attachment={name}"));
            }
        }
        Request::Mark { uid, state } => {
            let state = match state {
                ReadState::Read => "read",
                ReadState::Unread => "unread",
            };
            args.extend(["mark".into(), uid.to_string(), state.into()]);
        }
    }
    args
}

fn serve(identity: &str) -> Result<Option<ExitStatus>, Box<dyn std::error::Error>> {
    if !matches!(identity, "alpha" | "beta") {
        return Err(invalid("unregistered lab mailbox for SSH key").into());
    }
    let request = decode(
        PREFIX,
        &env::var("SSH_ORIGINAL_COMMAND").unwrap_or_default(),
    )?;
    if request == (Request::Whoami {}) {
        println!("{identity}@agentmail.test");
        return Ok(None);
    }
    if matches!(request, Request::WatchNotifications { .. }) && identity == "alpha" {
        notify::serve_gateway()?;
        return Ok(None);
    }
    if let Request::SessionStatus { scope, blob } = request {
        return Ok(Some(session::serve_status(scope, &blob)?));
    }
    mail(identity, request)
}

fn mail(
    identity: &str,
    request: Request,
) -> Result<Option<ExitStatus>, Box<dyn std::error::Error>> {
    if matches!(
        request,
        Request::PrepareSession { .. } | Request::SessionStatus { .. }
    ) {
        return Err(invalid("registration operations are unavailable with this key").into());
    }
    if matches!(request, Request::WatchNotifications { .. }) {
        return Err(invalid("notification watch requires a registered conversation").into());
    }
    let runtime_file = env::current_exe()?.with_file_name("runtime-dir");
    let runtime = std::fs::read_to_string(runtime_file)?;
    let runtime = std::path::Path::new(runtime.trim());
    if !runtime.is_absolute() || !runtime.is_dir() {
        return Err(invalid("invalid lab Podman runtime directory").into());
    }
    let status = Command::new("/usr/bin/podman")
        .env("XDG_RUNTIME_DIR", runtime)
        .args([
            "--cgroup-manager=cgroupfs",
            "exec",
            "-i",
            MAIL_CONTAINER,
            LAB_CLI,
            "--identity",
            identity,
        ])
        .args(cli_arguments(request))
        .status()?;
    Ok(Some(status))
}

impl Client {
    fn ssh(&self, key: &Path, watch: bool) -> Command {
        let mut ssh = Command::new("ssh");
        ssh.args(["-F", "/dev/null"]);
        if self.ipv4 {
            ssh.arg("-4");
        }
        if watch {
            ssh.args(["-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2"]);
        }
        let known_hosts = self
            .known_hosts
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        ssh.args(["-T", "-p", &self.port.to_string(), "-i"])
            .arg(key)
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "HostKeyAlgorithms=ssh-ed25519",
                "-o",
                "ControlPath=none",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
            ])
            .arg(format!("UserKnownHostsFile=\"{known_hosts}\""))
            .arg("--")
            .arg(format!("agentmail@{}", self.host));
        ssh
    }
}

fn run() -> Result<Option<ExitStatus>, Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let mode = args.next();
    if mode.as_deref() == Some("--local-notifications") {
        notify::local_snapshot(args.collect())?;
        return Ok(None);
    }
    if mode.as_deref() == Some("--notification-snapshot") {
        session::notification_snapshot(args.collect())?;
        return Ok(None);
    }
    if mode.as_deref() == Some("--serve-session") {
        return session::serve(args.collect());
    }
    if mode.as_deref() == Some("--serve") {
        let identity = args
            .next()
            .ok_or_else(|| invalid("missing server identity"))?;
        if args.next().is_some() {
            return Err(invalid("unexpected server argument").into());
        }
        return serve(&identity);
    }
    let mut client = Client::parse();
    if let Request::PrepareSession { identity, offline } = &client.request {
        session::prepare(&client, identity, *offline)?;
        return Ok(None);
    }
    let watch_projects = match &client.request {
        Request::WatchNotifications { pause, resume, .. } if *pause || *resume => {
            notify::set_paused(*pause)?;
            return Ok(None);
        }
        Request::WatchNotifications { project, .. } => Some(project.clone()),
        _ => None,
    };
    let scope = if watch_projects.is_some() {
        None
    } else {
        session::configured()?
    };
    let payload = prepare_upload(&mut client.request)?;
    let mut ssh = client.ssh(
        scope
            .as_ref()
            .map_or(client.key.as_path(), |scope| scope.key.as_path()),
        watch_projects.is_some(),
    );
    let ssh_request = match &scope {
        Some(scope) => encode(
            SESSION_PREFIX,
            &session::SessionRequest {
                scope: scope.binding.clone(),
                request: client.request,
            },
        )?,
        None => encode(PREFIX, &client.request)?,
    };
    ssh.arg(ssh_request);
    if let Some(projects) = watch_projects {
        notify::watch(ssh, projects)?;
        return Ok(None);
    }
    let status = if let Some(payload) = payload {
        let mut process = ssh.stdin(Stdio::piped()).spawn()?;
        let write_result = (|| {
            let mut input = process.stdin.take().expect("SSH stdin is piped");
            write_frame(&mut input, &payload.patch)?;
            if let Some(body) = payload.body {
                write_frame(&mut input, &body)?;
            }
            Ok::<_, io::Error>(())
        })();
        let status = process.wait()?;
        if status.success() {
            write_result?;
        }
        status
    } else {
        ssh.status()?
    };
    if status.code() == Some(255) {
        if let Some(scope) = scope {
            eprintln!("{} の SSH 接続に失敗しました. サンドボックス内なら承認を得て同じコマンドを再試行してください. 接続はできても未登録の場合は, prepare-session で VM の登録状態を確認してください.", scope.address());
        }
    }
    Ok(Some(status))
}

fn main() -> ExitCode {
    match run() {
        Ok(None) => ExitCode::SUCCESS,
        Ok(Some(status)) => ExitCode::from(
            status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1),
        ),
        Err(error) => {
            eprintln!("agentmail-ssh: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_command_metacharacters_as_data_and_limits_arguments() {
        let request = Request::Send {
            to: vec!["alpha".into()],
            cc: vec![],
            subject: Some("日本語 $(id); 'quoted'".into()),
            markdown: true,
            inline_patch: None,
            patch: None,
            upload: None,
        };
        let command = encode(PREFIX, &request).unwrap();
        assert!(command
            .strip_prefix(PREFIX)
            .unwrap()
            .bytes()
            .all(|b| b.is_ascii_hexdigit()));
        let decoded: Request = decode(PREFIX, &command).unwrap();
        assert_eq!(decoded, request);
        assert!(cli_arguments(decoded).contains(&"--subject=日本語 $(id); 'quoted'".to_owned()));
    }

    #[test]
    fn rejects_shell_commands_and_client_selected_identity() {
        assert!(decode::<Request>(PREFIX, "id").is_err());
        let request = br#"{"operation":"whoami","identity":"alpha"}"#;
        let command = PREFIX.to_owned()
            + &request
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
        assert!(decode::<Request>(PREFIX, &command).is_err());
        let forged = serde_json::json!({
            "operation": "send", "to": ["beta"], "cc": [], "markdown": false,
            "subject": "patch", "patch": "/etc/passwd",
        });
        assert!(decode::<Request>(PREFIX, &encode(PREFIX, &forged).unwrap()).is_err());
    }
}
