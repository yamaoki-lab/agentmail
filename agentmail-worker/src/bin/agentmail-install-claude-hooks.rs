use agentmail_worker::imap::invalid;
use agentmail_worker::runtime::atomic_write;
use clap::Parser;
use serde::de::{Deserialize, Deserializer, Error, MapAccess, SeqAccess, Visitor};
use serde_json::{json, Value};
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Preview or install Agentmail's user-level Claude Code hooks")]
struct Args {
    /// Write the settings file.
    #[arg(long)]
    apply: bool,
}

struct SettingsValue(Value);

impl<'de> Deserialize<'de> for SettingsValue {
    fn deserialize<D: Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        struct UniqueKeys;
        impl<'de> Visitor<'de> for UniqueKeys {
            type Value = SettingsValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("JSON with unique object keys")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut source: M) -> Result<Self::Value, M::Error> {
                let mut fields = serde_json::Map::new();
                while let Some((key, SettingsValue(value))) =
                    source.next_entry::<String, SettingsValue>()?
                {
                    if fields.insert(key, value).is_some() {
                        return Err(M::Error::custom("duplicate key in Claude settings"));
                    }
                }
                Ok(SettingsValue(Value::Object(fields)))
            }

            fn visit_seq<S: SeqAccess<'de>>(self, mut source: S) -> Result<Self::Value, S::Error> {
                let mut values = Vec::new();
                while let Some(SettingsValue(value)) = source.next_element()? {
                    values.push(value);
                }
                Ok(SettingsValue(Value::Array(values)))
            }

            fn visit_bool<E: Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(SettingsValue(Value::from(value)))
            }
            fn visit_i64<E: Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(SettingsValue(Value::from(value)))
            }
            fn visit_u64<E: Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(SettingsValue(Value::from(value)))
            }
            fn visit_f64<E: Error>(self, value: f64) -> Result<Self::Value, E> {
                Ok(SettingsValue(Value::from(value)))
            }
            fn visit_str<E: Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(SettingsValue(Value::from(value)))
            }
            fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
                Ok(SettingsValue(Value::Null))
            }
        }
        decoder.deserialize_any(UniqueKeys)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Args::parse();
    let home = PathBuf::from(env::var_os("HOME").ok_or_else(|| invalid("HOME is not set"))?);
    let path = home.join(".claude/settings.json");
    for name in ["agentmail-hook", "agentmail-ssh"] {
        let binary = home.join(".local/lib/agentmail").join(name);
        let info = binary.metadata()?;
        if !info.is_file() || info.permissions().mode() & 0o111 == 0 {
            return Err(invalid(format!(
                "install the Mac Agentmail client first; missing {}",
                binary.display()
            ))
            .into());
        }
    }
    let read = || match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {
            Ok((Some(fs::read(&path)?), meta.permissions().mode() & 0o777))
        }
        Ok(_) => Err(invalid("expected an ordinary Claude settings file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok((None, 0o600)),
        Err(error) => Err(error),
    };
    let (previous, mode) = read()?;
    let mut settings: Value = match &previous {
        Some(bytes) => serde_json::from_slice::<SettingsValue>(bytes)?.0,
        None => json!({}),
    };
    let hooks = settings
        .as_object_mut()
        .ok_or_else(|| invalid("settings must be a JSON object"))?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| invalid("hooks must be a JSON object"))?;
    let mut added = Vec::new();
    for (event, flag) in [("PostToolUse", ""), ("Stop", "--stop ")] {
        let command = format!(
            r#""$HOME/.local/lib/agentmail/agentmail-hook" {flag}--claude-registered "$HOME/.local/lib/agentmail/agentmail-ssh" "$HOME/.local/share/agentmail/notification-state""#
        );
        let groups = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| invalid(format!("{event} must contain a list of hook groups")))?;
        let mut configured = false;
        for group in groups.iter() {
            let handlers = group
                .get("hooks")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid(format!("invalid hook group under {event}")))?;
            if group
                .get("matcher")
                .is_none_or(|matcher| matcher.is_null() || matcher == "" || matcher == "*")
            {
                configured |= handlers.iter().any(|handler| {
                    handler.get("type").and_then(Value::as_str) == Some("command")
                        && handler.get("command").and_then(Value::as_str) == Some(command.as_str())
                });
            }
        }
        if !configured {
            groups.push(json!({"hooks": [{"type": "command", "command": command, "timeout": 5}]}));
            added.push(event);
        }
    }
    if added.is_empty() {
        println!("Agentmail hooks already configured in {}", path.display());
    } else if options.apply {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(path.parent().unwrap())?;
        let mut bytes = serde_json::to_vec_pretty(&settings)?;
        bytes.push(b'\n');
        if read()? != (previous, mode) {
            return Err(invalid("Claude settings changed; run the installer again").into());
        }
        atomic_write(&path, &bytes, mode, None)?;
        println!("Installed {} in {}", added.join(", "), path.display());
    } else {
        println!(
            "Would add {} to {}. Run with --apply to write.",
            added.join(", "),
            path.display()
        );
    }
    Ok(())
}
