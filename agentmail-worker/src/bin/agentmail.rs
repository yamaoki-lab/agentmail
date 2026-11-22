//! Trusted operator CLI for the isolated mail lab.
use agentmail_worker::imap::{invalid, Imap};
use agentmail_worker::upload::{
    expect_end, read_frame, read_limited, valid_filename, MAX_BODY, MAX_PATCH,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use lettre::message::header::{ContentTransferEncoding, ContentType};
use lettre::message::MessageBuilder;
use lettre::message::{Attachment, Body, Mailbox, MultiPart, SinglePart};
use lettre::{Message, SmtpTransport, Transport};
use mailparse::{DispositionType, MailHeaderMap, ParsedMail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Accounts = BTreeMap<String, String>;
const SMTP_HOST: &str = "127.0.0.1";
const IMAP: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 143);
const MAX_RECEIVED: usize = 10 * 1024 * 1024;

#[derive(Parser)]
#[command(about = "Send and read mail inside the isolated agentmail lab")]
struct Cli {
    #[arg(long, default_value = "/run/agentmail-lab/accounts.json", hide = true)]
    accounts: PathBuf,
    /// Short identity or full registered mail address.
    #[arg(long)]
    identity: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Send {
        #[arg(long, required = true)]
        to: Vec<String>,
        #[arg(long)]
        cc: Vec<String>,
        #[arg(long, required_unless_present_any = ["inline_patch", "upload_inline"])]
        subject: Option<String>,
        /// Send a single git format-patch message as the mail body.
        #[arg(long, conflicts_with_all = ["subject", "markdown", "patch", "upload_inline", "upload_attachment"])]
        inline_patch: Option<PathBuf>,
        #[arg(long, hide = true, conflicts_with_all = ["subject", "markdown", "patch", "upload_attachment"])]
        upload_inline: bool,
        #[command(flatten)]
        content: Content,
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
        #[command(flatten)]
        content: Content,
    },
    Export {
        uid: u32,
        /// Export bytes of the named attachment instead of the whole .eml.
        #[arg(long)]
        attachment: Option<String>,
    },
    Mark {
        uid: u32,
        state: ReadState,
    },
}

#[derive(Args)]
struct Content {
    /// Treat the UTF-8 body from stdin as text/markdown.
    #[arg(long)]
    markdown: bool,
    /// Attach the original bytes of a git format-patch file.
    #[arg(long)]
    patch: Option<PathBuf>,
    #[arg(long, hide = true, conflicts_with = "patch")]
    upload_attachment: Option<String>,
}

struct MailContent {
    markdown: bool,
    body: String,
    attachment: Option<(String, Vec<u8>)>,
}

impl Content {
    fn read(self, mut input: impl Read) -> Result<MailContent> {
        let (body, attachment) = if let Some(filename) = self.upload_attachment {
            if !valid_filename(&filename) {
                return Err(invalid("invalid patch attachment filename").into());
            }
            let patch = read_frame(&mut input, MAX_PATCH)?;
            let body = read_frame(&mut input, MAX_BODY)?;
            expect_end(&mut input)?;
            (body, Some((filename, patch)))
        } else {
            let body = read_limited(input, MAX_BODY)?;
            let attachment = if let Some(path) = self.patch {
                let filename = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| invalid("patch filename must be UTF-8"))?;
                header_value(filename)?;
                Some((
                    filename.to_owned(),
                    read_limited(File::open(&path)?, MAX_PATCH)?,
                ))
            } else {
                None
            };
            (body, attachment)
        };
        Ok(MailContent {
            markdown: self.markdown,
            body: String::from_utf8(body)?,
            attachment,
        })
    }
}

#[derive(Clone, ValueEnum)]
enum ReadState {
    Read,
    Unread,
}

#[derive(Serialize)]
struct InboxEntry {
    uid: u32,
    from: String,
    to: String,
    cc: String,
    subject: String,
    unread: bool,
}

fn account(accounts: &Accounts, name: &str) -> Result<String> {
    let normalized = name.trim().to_ascii_lowercase();
    let address = if normalized.contains('@') {
        normalized
    } else {
        format!("{normalized}@agentmail.test")
    };
    if accounts.contains_key(&address) {
        Ok(address)
    } else {
        Err(invalid(format!("unregistered lab identity: {name}")).into())
    }
}

fn header_value(value: &str) -> Result<()> {
    if value.chars().any(char::is_control) {
        return Err(invalid("header values must not contain control characters").into());
    }
    Ok(())
}

fn message_builder(
    from: &str,
    to: &[String],
    cc: &[String],
    subject: &str,
) -> Result<MessageBuilder> {
    header_value(subject)?;
    let mut builder = Message::builder()
        .from(from.parse::<Mailbox>()?)
        .subject(subject)
        .message_id(None);
    let mut recipients = BTreeSet::new();
    for address in to {
        if recipients.insert(address) {
            builder = builder.to(address.parse()?);
        }
    }
    for address in cc {
        if recipients.insert(address) {
            builder = builder.cc(address.parse()?);
        }
    }
    Ok(builder)
}

fn inline_git_patch(from: &str, to: &[String], cc: &[String], source: &[u8]) -> Result<Message> {
    let content = source
        .strip_prefix(b"From ")
        .and_then(|_| source.splitn(2, |byte| *byte == b'\n').nth(1))
        .ok_or_else(|| invalid("expected one git format-patch message"))?;
    let envelope_suffix = b" Mon Sep 17 00:00:00 2001";
    if content.split(|byte| *byte == b'\n').any(|line| {
        line.strip_suffix(b"\r")
            .unwrap_or(line)
            .ends_with(envelope_suffix)
            && line.starts_with(b"From ")
    }) {
        return Err(invalid("send one git format-patch message at a time").into());
    }
    let patch = mailparse::parse_mail(content)?;
    if patch.ctype.mimetype != "text/plain" {
        return Err(invalid("expected a text/plain git format-patch message").into());
    }
    let body = patch.get_body()?;
    if !body.lines().any(|line| line.starts_with("diff --git ")) {
        return Err(invalid("no Git diff in patch body").into());
    }
    let get_header = |name: &str| -> Result<String> {
        let value = patch
            .headers
            .get_first_value(name)
            .ok_or_else(|| invalid(format!("missing patch {name}")))?;
        header_value(&value)?;
        Ok(value)
    };
    let subject = get_header("Subject")?;
    let author = get_header("From")?;
    let _: Mailbox = author.parse()?;
    let author_date = get_header("Date")?;
    mailparse::dateparse(&author_date)?;
    // git mailinfo reads these in-body headers as the commit author and date.
    let inline = format!("From: {author}\nDate: {author_date}\n\n{body}");
    let part = SinglePart::builder()
        .header(ContentType::TEXT_PLAIN)
        .body(inline);
    Ok(message_builder(from, to, cc, &subject)?.singlepart(part)?)
}

fn build_mail(
    from: &str,
    to: &[String],
    cc: &[String],
    subject: &str,
    content: MailContent,
    reply: Option<(&str, &str)>,
) -> Result<Message> {
    let mut builder = message_builder(from, to, cc, subject)?;
    if let Some((parent, references)) = reply {
        header_value(parent)?;
        header_value(references)?;
        builder = builder
            .in_reply_to(parent.to_owned())
            .references(references.to_owned());
    }
    let format = if content.markdown {
        ContentType::parse("text/markdown; charset=utf-8")?
    } else {
        ContentType::TEXT_PLAIN
    };
    let part = SinglePart::builder().header(format).body(content.body);
    if let Some((filename, bytes)) = content.attachment {
        let encoded = Body::new_with_encoding(bytes, ContentTransferEncoding::Base64)
            .map_err(|_| invalid("failed to encode patch attachment"))?;
        let patch = Attachment::new(filename)
            .body(encoded, ContentType::parse("application/octet-stream")?);
        Ok(builder.multipart(MultiPart::mixed().singlepart(part).singlepart(patch))?)
    } else {
        Ok(builder.singlepart(part)?)
    }
}

fn send(message: &Message) -> Result<()> {
    let smtp = SmtpTransport::builder_dangerous(SMTP_HOST)
        .timeout(Some(std::time::Duration::from_secs(10)))
        .build();
    let message_id = message
        .headers()
        .get_raw("Message-ID")
        .ok_or_else(|| invalid("missing generated Message-ID"))?;
    smtp.send(message)?;
    println!("{message_id}");
    Ok(())
}

fn selected(accounts: &Accounts, identity: &str, writable: bool) -> Result<Imap> {
    let mut imap = Imap::login(IMAP, identity, &accounts[identity])?;
    imap.command(if writable {
        "SELECT INBOX"
    } else {
        "EXAMINE INBOX"
    })?;
    Ok(imap)
}

fn fetch_uid(line: &str) -> Option<u32> {
    let (_, fields) = line.split_once(" FETCH (")?;
    let mut previous = "";
    for word in fields.split_whitespace() {
        if previous == "UID" {
            return word.trim_end_matches(')').parse().ok();
        }
        previous = word;
    }
    None
}

fn inbox(mut imap: Imap, unread: bool) -> Result<Vec<InboxEntry>> {
    let result = imap.command(if unread {
        "UID SEARCH UNSEEN"
    } else {
        "UID SEARCH ALL"
    })?;
    let list = result
        .iter()
        .find_map(|r| {
            (r.line == "* SEARCH" || r.line.starts_with("* SEARCH ")).then(|| &r.line[8..])
        })
        .ok_or_else(|| invalid("missing IMAP SEARCH result"))?;
    let mut ids = list
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ids.sort_unstable_by(|left, right| right.cmp(left));
    ids.truncate(20);
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sequence = ids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let response = imap.command(&format!(
        "UID FETCH {sequence} (UID FLAGS BODY.PEEK[HEADER.FIELDS (FROM TO CC SUBJECT)])"
    ))?;
    let mut entries = Vec::new();
    for item in response {
        let Some(bytes) = item.literal else { continue };
        let uid = fetch_uid(&item.line).ok_or_else(|| invalid("missing IMAP UID"))?;
        let flags = item
            .line
            .split_once("FLAGS (")
            .and_then(|(_, s)| s.split_once(')'))
            .ok_or_else(|| invalid("missing IMAP FLAGS"))?
            .0;
        let (headers, _) = mailparse::parse_headers(&bytes)?;
        entries.push(InboxEntry {
            uid,
            from: headers.get_first_value("From").unwrap_or_default(),
            to: headers.get_first_value("To").unwrap_or_default(),
            cc: headers.get_first_value("Cc").unwrap_or_default(),
            subject: headers.get_first_value("Subject").unwrap_or_default(),
            unread: !flags
                .split_whitespace()
                .any(|flag| flag.eq_ignore_ascii_case("\\Seen")),
        });
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.uid));
    Ok(entries)
}

fn check_uid(uid: u32) -> Result<()> {
    if uid == 0 {
        return Err(invalid("IMAP UID must be positive").into());
    }
    Ok(())
}

fn fetch_message(mut imap: Imap, uid: u32) -> Result<Vec<u8>> {
    check_uid(uid)?;
    imap.set_literal_limit(MAX_RECEIVED);
    for item in imap.command(&format!("UID FETCH {uid} (UID BODY.PEEK[])"))? {
        if fetch_uid(&item.line) == Some(uid) {
            if let Some(body) = item.literal {
                return Ok(body);
            }
        }
    }
    Err(invalid("message UID not found").into())
}

fn display_part<'a>(mail: &'a ParsedMail<'a>) -> Option<&'a ParsedMail<'a>> {
    if mail.get_content_disposition().disposition == DispositionType::Attachment {
        return None;
    }
    match mail.ctype.mimetype.as_str() {
        "text/plain" | "text/markdown" => Some(mail),
        mime if mime.starts_with("multipart/") => mail.subparts.iter().find_map(display_part),
        _ => None,
    }
}

fn find_attachment<'a>(mail: &'a ParsedMail<'a>, name: &str) -> Option<&'a ParsedMail<'a>> {
    let disposition = mail.get_content_disposition();
    if disposition.disposition == DispositionType::Attachment
        && disposition
            .params
            .get("filename")
            .is_some_and(|filename| filename == name)
    {
        return Some(mail);
    }
    mail.subparts
        .iter()
        .find_map(|part| find_attachment(part, name))
}

fn inline_upload(mut input: impl Read) -> io::Result<Vec<u8>> {
    let bytes = read_frame(&mut input, MAX_PATCH)?;
    expect_end(&mut input)?;
    Ok(bytes)
}

fn recipient_addresses(accounts: &Accounts, names: &[String]) -> Result<Vec<String>> {
    names.iter().map(|name| account(accounts, name)).collect()
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let accounts: Accounts = serde_json::from_reader(File::open(cli.accounts)?)?;
    let sender = account(&accounts, &cli.identity)?;
    match cli.command {
        Command::Send {
            to,
            cc,
            subject,
            inline_patch,
            upload_inline,
            content,
        } => {
            let to = recipient_addresses(&accounts, &to)?;
            let cc = recipient_addresses(&accounts, &cc)?;
            let message = if let Some(path) = inline_patch {
                let bytes = read_limited(File::open(path)?, MAX_PATCH)?;
                inline_git_patch(&sender, &to, &cc, &bytes)?
            } else if upload_inline {
                inline_git_patch(&sender, &to, &cc, &inline_upload(io::stdin().lock())?)?
            } else {
                let subject = subject
                    .as_deref()
                    .ok_or_else(|| invalid("missing subject"))?;
                build_mail(
                    &sender,
                    &to,
                    &cc,
                    subject,
                    content.read(io::stdin().lock())?,
                    None,
                )?
            };
            send(&message)?;
        }
        Command::Inbox { unread } => {
            let entries = inbox(selected(&accounts, &sender, false)?, unread)?;
            println!("{}", serde_json::to_string_pretty(&entries)?);
        }
        Command::Read { uid } => {
            let raw = fetch_message(selected(&accounts, &sender, false)?, uid)?;
            let mail = mailparse::parse_mail(&raw)?;
            let part =
                display_part(&mail).ok_or_else(|| invalid("no plain text or Markdown body"))?;
            print!("{}", part.get_body()?);
        }
        Command::Reply { uid, content } => {
            let raw = fetch_message(selected(&accounts, &sender, false)?, uid)?;
            let mail = mailparse::parse_mail(&raw)?;
            let address: Mailbox = mail
                .headers
                .get_first_value("Reply-To")
                .or_else(|| mail.headers.get_first_value("From"))
                .ok_or_else(|| invalid("missing reply address"))?
                .parse()?;
            let recipient = account(&accounts, &address.email.to_string())?;
            let parent = mail
                .headers
                .get_first_value("Message-ID")
                .ok_or_else(|| invalid("missing Message-ID"))?;
            let references = match mail.headers.get_first_value("References") {
                Some(previous) => format!("{previous} {parent}"),
                None => parent.clone(),
            };
            let subject = mail.headers.get_first_value("Subject").unwrap_or_default();
            let subject = if subject
                .get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("re:"))
            {
                subject
            } else {
                format!("Re: {subject}")
            };
            send(&build_mail(
                &sender,
                &[recipient],
                &[],
                &subject,
                content.read(io::stdin().lock())?,
                Some((&parent, &references)),
            )?)?;
        }
        Command::Export { uid, attachment } => {
            let raw = fetch_message(selected(&accounts, &sender, false)?, uid)?;
            let bytes = match attachment {
                Some(name) => {
                    let mail = mailparse::parse_mail(&raw)?;
                    find_attachment(&mail, &name)
                        .ok_or_else(|| invalid("attachment not found"))?
                        .get_body_raw()?
                }
                None => raw,
            };
            io::stdout().lock().write_all(&bytes)?;
        }
        Command::Mark { uid, state } => {
            check_uid(uid)?;
            let mut imap = selected(&accounts, &sender, true)?;
            let response = imap.command(&format!("UID FETCH {uid} (UID FLAGS)"))?;
            if !response
                .iter()
                .any(|item| fetch_uid(&item.line) == Some(uid))
            {
                return Err(invalid("message UID not found").into());
            }
            let direction = match state {
                ReadState::Read => "+",
                ReadState::Unread => "-",
            };
            imap.command(&format!("UID STORE {uid} {direction}FLAGS.SILENT (\\Seen)"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_and_multiple_to_cc_are_encoded_with_a_unique_envelope() {
        let alpha = "alpha@agentmail.test".to_owned();
        let beta = "beta@agentmail.test".to_owned();
        let gamma = "gamma@agentmail.test".to_owned();
        let message = build_mail(
            &alpha,
            &[alpha.clone(), beta.clone()],
            &[beta.clone(), gamma.clone()],
            "日本語の件名",
            MailContent {
                markdown: true,
                body: "# Markdown\n\n本文".into(),
                attachment: None,
            },
            None,
        )
        .unwrap();
        let recipients: BTreeSet<_> = message
            .envelope()
            .to()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            recipients,
            [alpha, beta.clone(), gamma.clone()].into_iter().collect()
        );
        let bytes = message.formatted();
        let mail = mailparse::parse_mail(&bytes).unwrap();
        assert!(mail.headers.get_first_value("To").unwrap().contains(&beta));
        assert_eq!(mail.headers.get_first_value("Cc").unwrap(), gamma);
        assert_eq!(
            mail.headers.get_first_value("Subject").unwrap(),
            "日本語の件名"
        );
        let part = display_part(&mail).unwrap();
        assert_eq!(part.ctype.mimetype, "text/markdown");
        assert!(part.get_body().unwrap().contains("# Markdown"));
    }

    #[test]
    fn patch_attachment_preserves_original_line_endings_and_bytes() {
        let patch =
            std::env::temp_dir().join(format!("agentmail-patch-{}.patch", std::process::id()));
        let raw = b"From abc Mon Sep 17 00:00:00 2001\n\n+keep trailing space \n+keep CRLF\r\n";
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&patch)
            .unwrap();
        file.write_all(raw).unwrap();
        drop(file);
        let addr = "alpha@agentmail.test";
        let name = patch.file_name().unwrap().to_str().unwrap().to_owned();
        let content = Content {
            markdown: false,
            patch: Some(patch.clone()),
            upload_attachment: None,
        }
        .read(&b"See attached."[..])
        .unwrap();
        let message = build_mail(addr, &[addr.to_owned()], &[], "Patch", content, None);
        std::fs::remove_file(&patch).unwrap();
        let formatted = message.unwrap().formatted();
        let mail = mailparse::parse_mail(&formatted).unwrap();
        assert_eq!(
            find_attachment(&mail, &name)
                .unwrap()
                .get_body_raw()
                .unwrap(),
            raw
        );
    }

    #[test]
    fn reads_uploaded_attachment_and_rejects_incomplete_upload() {
        use agentmail_worker::upload::write_frame;
        let patch = b"+ligne avec retour\r\n";
        let body = "確認してください。\n";
        let mut upload = Vec::new();
        write_frame(&mut upload, patch).unwrap();
        write_frame(&mut upload, body.as_bytes()).unwrap();
        let content = || Content {
            markdown: true,
            patch: None,
            upload_attachment: Some("test.patch".into()),
        };
        let mail = content().read(upload.as_slice()).unwrap();
        assert_eq!(mail.body, body);
        assert!(mail.markdown);
        assert_eq!(mail.attachment, Some(("test.patch".into(), patch.to_vec())));
        upload.pop();
        assert!(content().read(upload.as_slice()).is_err());
    }

    #[test]
    fn inline_git_patch_applies_with_the_original_author() {
        use std::process::Command;
        let directory =
            std::env::temp_dir().join(format!("agentmail-inline-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&directory)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args([
                    "-c",
                    "user.name=Reviewer",
                    "-c",
                    "user.email=reviewer@agentmail.test",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        let patch = concat!(
            "From 0123456789012345678901234567890123456789 Mon Sep 17 00:00:00 2001\n",
            "From: Original Author <author@example.test>\n",
            "Date: Sun, 22 Nov 2026 00:01:00 +0900\n",
            "Subject: [PATCH] Add note\n\n",
            "Preserve this message.\n\n",
            "---\n",
            "diff --git a/note.txt b/note.txt\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/note.txt\n",
            "@@ -0,0 +1 @@\n",
            "+note\n",
        );
        let sender = "alpha@agentmail.test";
        let mut uploaded = Vec::new();
        agentmail_worker::upload::write_frame(&mut uploaded, patch.as_bytes()).unwrap();
        let message = inline_git_patch(
            sender,
            &["beta@agentmail.test".to_owned()],
            &[],
            &inline_upload(uploaded.as_slice()).unwrap(),
        )
        .unwrap();
        let raw = message.formatted();
        let mail = mailparse::parse_mail(&raw).unwrap();
        assert_eq!(
            mail.headers.get_first_value("From").as_deref(),
            Some(sender)
        );
        assert!(display_part(&mail)
            .unwrap()
            .get_body()
            .unwrap()
            .contains("diff --git "));
        assert!(mail.subparts.is_empty());
        std::fs::write(directory.join("inline.eml"), raw).unwrap();
        git(&["init", "-q"]);
        git(&["commit", "-qm", "base", "--allow-empty"]);
        git(&["am", "inline.eml"]);
        assert_eq!(git(&["log", "-1", "--format=%an <%ae>%n%aI%n%B"]).trim(),
                   "Original Author <author@example.test>\n2026-11-22T00:01:00+09:00\nAdd note\n\nPreserve this message.");
        assert_eq!(
            std::fs::read_to_string(directory.join("note.txt")).unwrap(),
            "note\n"
        );
        std::fs::remove_dir_all(directory).unwrap();
        let series = format!("{patch}{patch}");
        assert!(inline_git_patch(
            sender,
            &["beta@agentmail.test".to_owned()],
            &[],
            series.as_bytes()
        )
        .is_err());
    }

    #[test]
    fn rejects_header_line_injection() {
        let addr = "alpha@agentmail.test";
        assert!(build_mail(
            addr,
            &[addr.to_owned()],
            &[],
            "hello\r\nCc: beta@agentmail.test",
            MailContent {
                markdown: false,
                body: "test".into(),
                attachment: None,
            },
            None,
        )
        .is_err());
    }
}
