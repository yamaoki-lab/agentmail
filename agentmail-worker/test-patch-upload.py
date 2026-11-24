"""Check the actual SSH client command and byte stream without contacting the VM."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

SSH = """#!/usr/bin/env python3
import hashlib
import json
import sys

prefix, hex_request = sys.argv[-1].split()
assert prefix == "agentmail-v1"
request = json.loads(bytes.fromhex(hex_request))
raw = sys.stdin.buffer.read()
parts = []
while raw:
    assert len(raw) >= 8
    length = int.from_bytes(raw[:8], "big")
    assert len(raw) >= 8 + length
    parts.append(hashlib.sha256(raw[8:8+length]).hexdigest())
    raw = raw[8+length:]
print(json.dumps([request, parts]))
"""


def exercise(root, binary):
    path = root / "example.patch"
    patch = b"+keep trailing space \r\n+another line\n"
    body = "説明は Markdown です.\n".encode()
    path.write_bytes(patch)
    shim = root / "ssh"
    shim.write_text(SSH)
    shim.chmod(0o700)
    env = {**os.environ, "PATH": f"{root}:{os.environ['PATH']}"}
    env.pop("CODEX_THREAD_ID", None)
    env.pop("CLAUDE_CODE_SESSION_ID", None)
    client = [binary, "--key", "/unused", "--known-hosts", "/unused"]

    def run(*args, message=b""):
        return json.loads(
            subprocess.check_output([*client, *args], input=message, cwd=root, env=env)
        )

    patch_hash = hashlib.sha256(patch).hexdigest()
    body_hash = hashlib.sha256(body).hexdigest()
    request, frames = run("send", "--to", "beta", "--inline-patch", path, message=b"ignored")
    assert request["upload"] == {"kind": "inline"}
    assert str(path) not in json.dumps(request)
    assert frames == [patch_hash]
    request, frames = run(
        "send", "--to", "beta", "--cc", "alpha", "--subject", "パッチ", "--markdown",
        "--patch", path, message=body,
    )
    assert request["upload"] == {"kind": "attachment", "filename": path.name}
    assert str(path) not in json.dumps(request)
    assert request["cc"] == ["alpha"] and request["markdown"]
    assert frames == [patch_hash, body_hash]
    request, frames = run("reply", "1", "--patch", path, message=body)
    assert request["upload"] == {"filename": path.name}
    assert frames == [patch_hash, body_hash]
    request, frames = run("send", "--to", "beta", "--subject", "Plain")
    assert request == {
        "operation": "send", "to": ["beta"], "cc": [], "subject": "Plain", "markdown": False,
    }
    assert not frames

    invalid = subprocess.run(
        [*client, "send", "--to", "beta", "--subject", "Conflict", "--inline-patch", path],
        cwd=root, env=env, capture_output=True,
    )
    assert invalid.returncode and not invalid.stdout


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    options = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="agentmail-patch-upload-") as directory:
        exercise(Path(directory), options.binary.resolve())
    print("PASS: client sends bounded patch frames, never the path, and preserves legacy mail requests")
