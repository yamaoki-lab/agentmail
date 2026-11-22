import email
import imaplib
import json
import smtplib
import time
from email import policy
from email.message import EmailMessage
from email.utils import make_msgid
from pathlib import Path

HOST = "127.0.0.1"
ALPHA, BETA = "alpha@agentmail.test", "beta@agentmail.test"


def wait_for_services():
    deadline = time.monotonic() + 30
    while True:
        try:
            with smtplib.SMTP(HOST, timeout=2) as smtp:
                smtp.noop()
            with imaplib.IMAP4(HOST, timeout=2) as imap:
                imap.noop()
            return
        except (OSError, smtplib.SMTPException, imaplib.IMAP4.error):
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.5)


def send(sender, recipient, subject, body, reply_to=None):
    message = EmailMessage(policy=policy.SMTP)
    message["From"] = sender
    message["To"] = recipient
    message["Subject"] = subject
    message["Message-ID"] = make_msgid(domain="agentmail.test")
    if reply_to:
        message["In-Reply-To"] = reply_to
        message["References"] = reply_to
    message.set_content(body)
    with smtplib.SMTP(HOST, timeout=5) as smtp:
        smtp.send_message(message)
    return str(message["Message-ID"])


def expect_ok(response):
    status, data = response
    if status != "OK":
        raise RuntimeError(f"IMAP returned {status}: {data}")
    return data


def receive(imap, message_id, sender, recipient, body, reply_to=None):
    deadline = time.monotonic() + 30
    while True:
        expect_ok(imap.noop())
        data = expect_ok(imap.uid("SEARCH", None, "UNSEEN", "HEADER", "Message-ID", message_id))
        ids = data[0].split()
        if len(ids) == 1:
            break
        if len(ids) > 1 or time.monotonic() >= deadline:
            raise RuntimeError(f"Expected exactly one unread copy of {message_id}; found {len(ids)}")
        time.sleep(0.5)

    uid = ids[0]
    fetched = expect_ok(imap.uid("FETCH", uid, "(BODY.PEEK[])"))
    raw = next(item[1] for item in fetched if isinstance(item, tuple))
    message = email.message_from_bytes(raw, policy=policy.default)
    assert message["From"] == sender and message["To"] == recipient
    assert message["Message-ID"] == message_id
    assert message.get_body().get_content().strip() == body
    assert message["In-Reply-To"] == reply_to
    assert uid in expect_ok(imap.uid("SEARCH", None, "UNSEEN"))[0].split()
    expect_ok(imap.uid("STORE", uid, "+FLAGS.SILENT", "(\\Seen)"))
    assert uid not in expect_ok(imap.uid("SEARCH", None, "UNSEEN"))[0].split()


def imap_session(address, accounts):
    imap = imaplib.IMAP4(HOST, timeout=5)
    imap.login(address, accounts[address])
    expect_ok(imap.select("INBOX"))
    return imap


def main():
    wait_for_services()
    accounts = json.loads(Path("/run/agentmail-lab/accounts.json").read_text())
    with imap_session(ALPHA, accounts) as alpha, imap_session(BETA, accounts) as beta:
        capabilities = b" ".join(expect_ok(beta.capability())).split()
        print(f"IMAP NOTIFY advertised after login: {'yes' if b'NOTIFY' in capabilities else 'no'}")
        first = send(ALPHA, BETA, "Agentmail smoke test", "hello beta")
        receive(beta, first, ALPHA, BETA, "hello beta")
        print("alpha -> beta: delivered, read with BODY.PEEK, then marked seen")
        reply = send(BETA, ALPHA, "Re: Agentmail smoke test", "hello alpha", reply_to=first)
        receive(alpha, reply, BETA, ALPHA, "hello alpha", reply_to=first)
        print("beta -> alpha: reply delivered with In-Reply-To, then marked seen")

    try:
        send(ALPHA, "missing@agentmail.test", "Unknown recipient", "should be rejected")
    except smtplib.SMTPRecipientsRefused as error:
        code, _ = error.recipients["missing@agentmail.test"]
        if not 500 <= code < 600:
            raise
    else:
        raise RuntimeError("Postfix accepted an unknown recipient")
    print("Unknown recipient rejected")
    for user in ("alpha", "beta"):
        assert any((Path("/data") / user / "Maildir" / "cur").iterdir())
    print("PASS: both messages are stored in Maildir")


if __name__ == "__main__":
    main()
