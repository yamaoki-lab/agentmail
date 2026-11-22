"""Check that Dovecot pushes an INBOX event before the client polls it."""

import json
import re
import socket
import sys
import time
from pathlib import Path

sys.path.insert(0, "/opt/mail-lab")
from smoke import ALPHA, BETA, HOST, expect_ok, imap_session, receive, send

STATUS = re.compile(rb'^\* STATUS "?INBOX"? \(', re.IGNORECASE)
UIDNEXT = re.compile(rb'\bUIDNEXT (\d+)\b', re.IGNORECASE)
SUBJECT = "Agentmail IMAP NOTIFY probe"


def main():
    accounts = json.loads(Path("/run/agentmail-lab/accounts.json").read_text())
    with socket.create_connection((HOST, 143), timeout=10) as connection:
        with connection.makefile("rb") as stream:
            def read_line():
                line = stream.readline(65536)
                if not line:
                    raise RuntimeError("Dovecot closed the IMAP connection")
                return line.rstrip(b"\r\n")

            def command(tag, request):
                connection.sendall(f"{tag} {request}\r\n".encode("ascii"))
                prefix = tag.encode() + b" "
                lines = []
                while True:
                    line = read_line()
                    if line.startswith(prefix):
                        if not line.startswith(prefix + b"OK "):
                            raise RuntimeError(f"{tag} failed: {line.decode(errors='replace')}")
                        return lines
                    lines.append(line)

            if not read_line().startswith(b"* OK "):
                raise RuntimeError("Unexpected IMAP greeting")
            command("a1", f"LOGIN {json.dumps(BETA)} {json.dumps(accounts[BETA])}")
            capabilities = command("a2", "CAPABILITY")
            if not any(b"NOTIFY" in line.split()[2:] for line in capabilities):
                raise RuntimeError("NOTIFY is not advertised after LOGIN")

            baseline = command("a3", "STATUS INBOX (UIDNEXT)")
            next_uid = next(int(match[1]) for line in baseline if STATUS.match(line)
                            if (match := UIDNEXT.search(line)))
            command("a4", "NOTIFY SET (MAILBOXES INBOX (MessageNew MessageExpunge))")
            print(f"Subscribed to INBOX without SELECT or IDLE (UIDNEXT {next_uid})", flush=True)

            started = time.monotonic()
            message_id = send(ALPHA, BETA, SUBJECT, "notify probe")
            while True:
                line = read_line()
                if STATUS.match(line) and (match := UIDNEXT.search(line)):
                    if int(match[1]) > next_uid:
                        print(f"Unsolicited event after {time.monotonic() - started:.3f}s: "
                              f"{line.decode(errors='replace')}", flush=True)
                        break
            command("a5", "LOGOUT")

    with imap_session(BETA, accounts) as imap:
        matches = expect_ok(imap.uid("SEARCH", None, "HEADER", "Message-ID", message_id))[0].split()
        if matches != [str(next_uid).encode()]:
            raise RuntimeError("New-mail event could not be tied to this probe message")
        receive(imap, message_id, ALPHA, BETA, "notify probe")
    print("PASS: IMAP NOTIFY pushed the probe delivery before any follow-up command")


if __name__ == "__main__":
    main()
