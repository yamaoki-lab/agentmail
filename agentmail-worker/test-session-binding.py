"""Exercise registration and the actual gateway without touching the VM."""

import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile

CODEX = "11111111-1111-1111-1111-111111111111"
CLAUDE = "22222222-2222-2222-2222-222222222222"


def call(arguments, env, cwd, returncode=0):
    result = subprocess.run(arguments, env=env, cwd=cwd, capture_output=True, text=True)
    assert result.returncode == returncode, result.stderr
    return result


def exercise(root, binary):
    home = root / "home"
    ssh = home / ".ssh"
    ssh.mkdir(parents=True)
    gateway = home / "gateway" / "agentmail-ssh"
    gateway.parent.mkdir()
    shutil.copy(binary, gateway)
    admin = gateway.with_name("agentmail-admin")
    shutil.copy(binary.with_name("agentmail-admin"), admin)
    keyfile = ssh / "authorized_keys"
    legacy = '# an unrelated key entry'
    keyfile.write_text(f"{legacy}\n")

    repo = root / "project"
    repo.mkdir()
    env = {**os.environ, "HOME": str(home)}
    env.pop("CODEX_THREAD_ID", None)
    env.pop("CLAUDE_CODE_SESSION_ID", None)
    codex = {**env, "CODEX_THREAD_ID": CODEX}
    claude = {**env, "CLAUDE_CODE_SESSION_ID": CLAUDE}
    client = [gateway, "--key", "/unused", "--known-hosts", "/unused"]
    call(["git", "init", "-q"], env, repo)
    with (repo / ".git/info/exclude").open("a") as exclude:
        exclude.write("\n/.agentmail/\n")

    def prepare(session_env, cwd):
        command = call([*client, "prepare-session", "--offline", "lan-check"], session_env, cwd)
        lines = command.stdout.splitlines()
        assert "manage-vm-mailboxes.sh identity create lan-check" in lines[2]
        assert " bind " in lines[4]
        name, tool, uuid, blob = shlex.split(lines[4])[-4:]
        return {"name": name, "tool": tool, "uuid": uuid, "blob": blob}

    def seed_registration(record):
        name, tool, uuid, blob = record.values()
        line = (
            f'restrict,command="{gateway} --serve-session {name} {tool} {uuid} {blob}" '
            f'ssh-ed25519 {blob} agentmail-session'
        )
        keyfile.write_text(f"{legacy}\n{line}\n")

    before = prepare(codex, root)
    after = prepare(claude, root)
    sessions = home / ".local/share/agentmail/sessions"
    assert sessions.stat().st_mode & 0o777 == 0o700
    codex_config = sessions / f"codex-{CODEX}"
    assert codex_config.stat().st_mode & 0o777 == 0o600
    assert not (repo / ".agentmail").exists()
    keys = home / ".local/share/agentmail/keys"
    codex_key_dir = keys / f"codex-{CODEX}"
    claude_key_dir = keys / f"claude-{CLAUDE}"
    assert (codex_key_dir / "id_ed25519").is_file()
    assert (claude_key_dir / "id_ed25519").is_file()
    assert claude_key_dir.stat().st_mode & 0o777 == 0o700
    assert (claude_key_dir / "id_ed25519").stat().st_mode & 0o777 == 0o600
    legacy_dir = ssh / "agentmail-sessions"
    assert not legacy_dir.exists()
    legacy_dir.mkdir()
    legacy_codex_dir = legacy_dir / codex_key_dir.name
    codex_key_dir.rename(legacy_codex_dir)
    old_config = repo / ".agentmail/sessions" / codex_config.name
    old_config.parent.mkdir(parents=True)
    codex_config.rename(old_config)
    call([*client, "prepare-session", "other"], codex, repo, 1)
    assert not codex_config.exists()
    assert prepare(codex, repo) == before
    assert codex_config.read_text() == old_config.read_text() == "lan-check\n"
    assert prepare(codex, root) == before
    assert not codex_key_dir.exists()
    assert before["blob"] != after["blob"]
    call([*client, "prepare-session", "other"], codex, repo, 1)
    seed_registration(before)
    assert call([admin, "list"], env, repo).stdout == f"lan-check@agentmail.test codex {CODEX}\n"
    assert "already assigned" in call([admin, "bind", *before.values()], env, repo).stdout
    another = {**after, "name": "another"}

    def forced_command(record):
        return [gateway, "--serve-session", *record.values()]

    def whoami(record, claim=None, expected=0):
        claim = record if claim is None else claim
        request = {
            "scope": {"identity": claim["name"], "tool": claim["tool"], "uuid": claim["uuid"]},
            "request": {"operation": "whoami"},
        }
        command = "agentmail-session-v1 " + json.dumps(request).encode().hex()
        return call(forced_command(record), {**env, "SSH_ORIGINAL_COMMAND": command}, repo, expected)

    assert f"codex {CODEX}, registered" in whoami(before).stdout
    assert "does not match" in whoami(before, {**before, "name": "beta"}, 1).stderr
    assert "does not match" in whoami(before, after, 1).stderr
    for arguments, error in [
        (["bind", *after.values()], "use rebind"),
        (["bind", "other", before["tool"], before["uuid"], before["blob"]], "already has a mailbox"),
        (["rebind", after["name"], "claude", CLAUDE, *list(after.values())[1:]], "previous conversation"),
        (["rebind", after["name"], "codex", CODEX, after["tool"], CLAUDE, before["blob"]], "SSH key already exists"),
    ]:
        assert error in call([admin, *arguments], env, repo, 1).stderr
    seed_registration(after)
    assert "revoked" in whoami(before, expected=1).stderr
    assert f"claude {CLAUDE}, registered" in whoami(after).stdout
    assert call([admin, "list"], env, repo).stdout == f"lan-check@agentmail.test claude {CLAUDE}\n"

    # Exercise client encoding over the same forced command using an SSH test double.
    shim = root / "ssh"
    shim.write_text(
        "#!/usr/bin/env python3\n"
        "import json, os, sys\n"
        "assert sys.argv[1:3] == ['-F', '/dev/null']\n"
        "if sys.argv[sys.argv.index('-i') + 1] == '/unused':\n"
        "    prefix, request = sys.argv[-1].split()\n"
        "    assert prefix == 'agentmail-v1'\n"
        "    assert json.loads(bytes.fromhex(request))['operation'] == 'session_status'\n"
        "    print(os.environ['TEST_STATUS'])\n"
        "    sys.exit(0)\n"
        "assert sys.argv[sys.argv.index('-i') + 1] == os.environ['TEST_KEY']\n"
        "args = json.loads(os.environ['TEST_FORCED_COMMAND'])\n"
        "os.environ['SSH_ORIGINAL_COMMAND'] = sys.argv[-1]\n"
        "os.execv(args[0], args)\n"
    )
    shim.chmod(0o700)
    test_env = {
        **claude,
        "PATH": f"{root}:{env['PATH']}",
        "TEST_KEY": str(claude_key_dir / "id_ed25519"),
        "TEST_FORCED_COMMAND": json.dumps([str(arg) for arg in forced_command(after)]),
    }
    assert f"claude {CLAUDE}, registered" in call([*client, "whoami"], test_env, root).stdout
    for state, create, bind, rebind, registered in [
        ({"state": "missing"}, True, True, False, False),
        ({"state": "unassigned"}, False, True, False, False),
        ({"state": "assigned", "old_tool": "codex", "old_uuid": CODEX}, False, False, True, False),
        ({"state": "registered"}, False, False, False, True),
    ]:
        online = {**test_env, "TEST_STATUS": json.dumps(state)}
        output = call([*client, "prepare-session", "lan-check"], online, root).stdout
        assert (" identity create " in output) == create
        assert (" bind " in output) == bind
        assert (" rebind " in output) == rebind
        assert (f"claude {CLAUDE}, registered" in output) == registered
        if create:
            assert output.index(" identity create ") < output.index(" bind ")
    unregistered = {**test_env, "CLAUDE_CODE_SESSION_ID": CODEX}
    assert "prepare-session" in call([*client, "whoami"], unregistered, root, 1).stderr

    legacy_request = "agentmail-v1 " + b'{"operation":"whoami"}'.hex()
    legacy_env = {**env, "SSH_ORIGINAL_COMMAND": legacy_request}
    assert call([gateway, "--serve", "beta"], legacy_env, root).stdout == "beta@agentmail.test\n"

    status_request = {
        "operation": "session_status",
        "scope": {"identity": another["name"], "tool": another["tool"], "uuid": another["uuid"]},
        "blob": another["blob"],
    }
    status_env = {
        **env,
        "SSH_ORIGINAL_COMMAND": "agentmail-v1 " + json.dumps(status_request).encode().hex(),
    }
    response = call([gateway, "--serve", "beta"], status_env, root)
    assert json.loads(response.stdout) == {"state": "conversation_in_use"}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="built agentmail-ssh next to built agentmail-admin")
    options = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="agentmail-binding-test-") as temporary:
        exercise(Path(temporary), options.binary.resolve())
    print("PASS: session keys, scope checks, conflicts, binding handoff, and legacy gateway")
