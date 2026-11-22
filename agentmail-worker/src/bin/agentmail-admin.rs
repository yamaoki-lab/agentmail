use agentmail_worker::imap::invalid;
use agentmail_worker::runtime::{
    atomic_write, registrations, unix_lock, valid_blob, Assignment, Scope,
};
use agentmail_worker::valid_uuid;
use clap::{Parser, Subcommand};
use serde_json::json;
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Parser)]
#[command(about = "Manage local Agentmail conversation assignments")]
struct Args {
    #[command(subcommand)]
    operation: Operation,
}

#[derive(Subcommand)]
enum Operation {
    List,
    Status {
        name: String,
        tool: String,
        uuid: String,
        blob: String,
    },
    Bind {
        name: String,
        tool: String,
        uuid: String,
        blob: String,
    },
    Rebind {
        name: String,
        old_tool: String,
        old_uuid: String,
        tool: String,
        uuid: String,
        blob: String,
    },
}

fn mailbox_exists(gateway: &Path, identity: &str) -> io::Result<bool> {
    if matches!(identity, "alpha" | "beta") {
        return Err(invalid("alpha and beta use dedicated fixed keys"));
    }
    let runtime = fs::read_to_string(gateway.with_file_name("runtime-dir"))?;
    let runtime = Path::new(runtime.trim());
    if !runtime.is_absolute() || !runtime.is_dir() {
        return Err(invalid("invalid Podman runtime directory"));
    }
    let result = agentmail_worker::runtime::output(
        Command::new("/usr/bin/podman")
            .env("XDG_RUNTIME_DIR", runtime)
            .args([
                "--cgroup-manager=cgroupfs",
                "exec",
                "agentmail-mail-lab",
                "/opt/mail-lab/agentmail-mailboxes",
                "list",
            ]),
    )?;
    let address = format!("{identity}@agentmail.test");
    Ok(result.lines().any(|line| line == address))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let operation = Args::parse().operation;
    let admin = env::current_exe()?;
    let gateway = admin.with_file_name("agentmail-ssh");
    let home = admin
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| invalid("expected the gateway's installation directory"))?;
    // SAFETY: geteuid reads the process credentials.
    if env::var_os("HOME").map(PathBuf::from).as_deref() != Some(home)
        || gateway.metadata()?.uid() != unsafe { libc::geteuid() }
    {
        return Err(invalid("run with sudo -iu agentmail").into());
    }
    execute(operation, home, &gateway, |identity| mailbox_exists(&gateway, identity))
}

fn execute(
    operation: Operation,
    home: &Path,
    gateway: &Path,
    mailbox_exists: impl Fn(&str) -> io::Result<bool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (scope, blob, old, status) = match operation {
        Operation::List => {
            let contents = fs::read_to_string(home.join(".ssh/authorized_keys"))?;
            let entries = registrations(&contents.lines().collect::<Vec<_>>(), gateway)?;
            for (_, entry) in entries.into_values() {
                println!(
                    "{}@agentmail.test {} {}",
                    entry.scope.identity, entry.scope.tool, entry.scope.uuid
                );
            }
            return Ok(());
        }
        Operation::Status {
            name,
            tool,
            uuid,
            blob,
        } => (
            Scope {
                identity: name,
                tool,
                uuid,
            },
            blob,
            None,
            true,
        ),
        Operation::Bind {
            name,
            tool,
            uuid,
            blob,
        } => (
            Scope {
                identity: name,
                tool,
                uuid,
            },
            blob,
            None,
            false,
        ),
        Operation::Rebind {
            name,
            old_tool,
            old_uuid,
            tool,
            uuid,
            blob,
        } => {
            if !matches!(old_tool.as_str(), "codex" | "claude") || !valid_uuid(&old_uuid) {
                return Err(invalid("invalid previous conversation").into());
            }
            (
                Scope {
                    identity: name,
                    tool,
                    uuid,
                },
                blob,
                Some((old_tool, old_uuid)),
                false,
            )
        }
    };
    if !scope.valid() || !valid_blob(&blob) {
        return Err(invalid("invalid identity, conversation ID, or Ed25519 key").into());
    }
    let requested = Assignment { scope, blob };
    let identity = &requested.scope.identity;
    let _lock = if status {
        None
    } else {
        Some(unix_lock(
            &home.join(".ssh/agentmail-operation.lock"),
            libc::LOCK_EX,
        )?)
    };
    let keys = home.join(".ssh/authorized_keys");
    let contents = fs::read_to_string(&keys)?;
    let mut lines: Vec<_> = contents.lines().collect();
    let entries = registrations(&lines, gateway)?;
    let current = entries.get(identity);
    let uuid_matches = |entry: &Assignment| {
        entry.scope.tool == requested.scope.tool && entry.scope.uuid == requested.scope.uuid
    };
    let conflict = entries
        .values()
        .any(|(_, entry)| uuid_matches(entry) && entry != &requested);
    if status {
        let result = if conflict {
            json!({"state": "conversation_in_use"})
        } else if !mailbox_exists(identity)? {
            if current.is_some() {
                return Err(invalid("SSH registration exists but the mailbox is missing").into());
            }
            json!({"state": "missing"})
        } else if let Some((_, entry)) = current {
            if entry == &requested {
                json!({"state": "registered"})
            } else {
                json!({"state": "assigned", "old_tool": entry.scope.tool, "old_uuid": entry.scope.uuid})
            }
        } else {
            json!({"state": "unassigned"})
        };
        println!("{result}");
        return Ok(());
    }
    match (old, current) {
        (None, Some((_, entry))) if entry == &requested => {
            println!(
                "READY: {identity}@agentmail.test already assigned to {} {}",
                requested.scope.tool, requested.scope.uuid
            );
            return Ok(());
        }
        (None, Some(_)) => return Err(invalid("mailbox is assigned; use rebind").into()),
        (Some((tool, uuid)), Some((_, entry)))
            if tool == entry.scope.tool && uuid == entry.scope.uuid =>
        {
            if uuid_matches(entry) {
                return Err(invalid("rebind requires a different conversation").into());
            }
        }
        (Some(_), _) => {
            return Err(
                invalid("current assignment does not match the previous conversation").into(),
            )
        }
        (None, None) => {}
    }
    if conflict {
        return Err(invalid("this conversation already has a mailbox").into());
    }
    if lines
        .iter()
        .filter(|line| !line.trim_start().starts_with('#'))
        .any(|line| {
            let mut fields = line.split_whitespace();
            fields.any(|field| field == "ssh-ed25519")
                && fields.next() == Some(requested.blob.as_str())
        })
    {
        return Err(invalid("this SSH key already exists; use a new session key").into());
    }
    if !mailbox_exists(identity)? {
        return Err(invalid(format!("create the mailbox {identity} on the VM first")).into());
    }
    let line = requested.authorized_key(gateway);
    if let Some((index, _)) = current {
        lines[*index] = &line;
    } else {
        lines.push(&line);
    }
    let mut updated = lines.join("\n");
    updated.push('\n');
    atomic_write(&keys, updated.as_bytes(), 0o600, None)?;
    println!(
        "READY: {identity}@agentmail.test assigned to {} {}",
        requested.scope.tool, requested.scope.uuid
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentmail_worker::runtime::TemporaryDirectory;

    #[test]
    fn binds_and_transfers_an_existing_mailbox() {
        let home = TemporaryDirectory::new(&env::temp_dir(), "agentmail-admin-test.").unwrap();
        let home = home.path();
        let gateway = home.join("gateway/agentmail-ssh");
        fs::create_dir(home.join(".ssh")).unwrap();
        let keys = home.join(".ssh/authorized_keys");
        let unrelated = "# an unrelated key entry\n";
        fs::write(&keys, unrelated).unwrap();
        let codex = "11111111-1111-1111-1111-111111111111";
        let claude = "22222222-2222-2222-2222-222222222222";
        let key = format!("AAAAC3NzaC1lZDI1NTE5AAAAI{}", "A".repeat(43));
        let new_key = format!("AAAAC3NzaC1lZDI1NTE5AAAAI{}", "B".repeat(43));
        let bind = || Operation::Bind {
            name: "lan-check".into(),
            tool: "codex".into(),
            uuid: codex.into(),
            blob: key.clone(),
        };
        let rebind = |blob: String| Operation::Rebind {
            name: "lan-check".into(),
            old_tool: "codex".into(),
            old_uuid: codex.into(),
            tool: "claude".into(),
            uuid: claude.into(),
            blob,
        };
        let exists = |identity: &str| Ok(identity == "lan-check");

        assert!(execute(bind(), home, &gateway, |_| Ok(false)).is_err());
        assert_eq!(fs::read_to_string(&keys).unwrap(), unrelated);
        execute(bind(), home, &gateway, exists).unwrap();
        let original = fs::read_to_string(&keys).unwrap();
        execute(bind(), home, &gateway, exists).unwrap();
        assert_eq!(fs::read_to_string(&keys).unwrap(), original);
        assert!(execute(rebind(key), home, &gateway, exists).is_err());
        assert_eq!(fs::read_to_string(&keys).unwrap(), original);

        execute(rebind(new_key.clone()), home, &gateway, exists).unwrap();
        let updated = fs::read_to_string(&keys).unwrap();
        assert!(updated.starts_with(unrelated));
        let entries = registrations(&updated.lines().collect::<Vec<_>>(), &gateway).unwrap();
        let assignment = &entries["lan-check"].1;
        assert_eq!(entries.len(), 1);
        assert_eq!(assignment.scope.tool, "claude");
        assert_eq!(assignment.scope.uuid, claude);
        assert_eq!(assignment.blob, new_key);
        assert_eq!(fs::metadata(&keys).unwrap().mode() & 0o777, 0o600);
    }
}
