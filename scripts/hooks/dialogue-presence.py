#!/usr/bin/env python3
"""Hold this Claude Code session's etcd-lease presence key while the session lives.

This is the cc half of the registration model in
`specs/plans/mu-dialogue-push-mailbox-v1.md` §1, implemented in
`crates/mu-dialogue/src/presence.rs`. That module's own docs name both writers
it expects -- "mu daemon per session; the cc Stop-hook watch process for Claude
Code peers" -- and only the mu half existed. Without a cc key, a Claude Code
peer falls back to mu-dialogue's activity-derived presence, whose default TTL is
one hour, so a live-but-quiet session reads as DEAD. Measured consequence
(2026-10-07): the IRC gateway saw the peer leave discovery, quit its puppet, and
the operator's reply hit "No such nick" on a session that was alive the whole
time. Bead mu-34mgd.

WHY A LEASE AND NOT MORE ACTIVITY. Activity-derived presence is only as reliable
as something that keeps poking it. The Stop-hook poller does refresh activity
while it runs, but it caps one idle watch at 30 minutes and only Stop re-arms it
-- so a session idle longer than that has nothing refreshing anything. A lease
depends on this process being alive and expires on its own when it is not. The
LEASE is the liveness proof; presence.rs says so explicitly and treats the key's
value as advisory.

CONTRACT WITH THE READER (presence.rs):
  key   <prefix>cc:<session-id>, default prefix /mu/dialogue/v1/peers/
  value advisory JSON; a malformed value still counts as live, and `role`
        falls back to the peer id's own role. We write the documented shape
        anyway so `dialogue_peers` reports a registered_at.
  truth the key exists <=> the peer is live NOW.

LABEL. If ~/.cache/dialogue-label-<session-id> exists, its text goes into the
value as `label`, which `agent dialogue peers` shows: a few words saying what
the session is working on. Anything may write that file (sprint-start, a slash
command, the operator by hand), so this holder knows nothing about beads or
titles. It is re-read on every renew, and a change is put under the SAME lease,
so relabelling never touches liveness. The reader bounds and sanitizes the
label again, since this is not the only writer of these keys.

FAIL-OPEN, the same convention presence.rs uses on its read side: anything
unconfigured or unreachable exits quietly and leaves activity-derived presence
to carry on. A monitoring gap must never break messaging.

LIFETIME. Watches the pid given as --watch-pid (the Claude Code process; the
SessionStart hook passes its own parent) and exits when it is gone, revoking the
lease so the key clears at once rather than at TTL. Guards against pid reuse by
remembering the watched process's command and re-checking it. If the pid cannot
be verified as Claude Code, it registers once and exits rather than looping
forever -- a key held by a dead keeper would be exactly the lie this fixes.

Usage (from SessionStart, backgrounded):
  dialogue-presence.py --session-id <sid> --watch-pid <ppid> &
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import signal
import stat
import subprocess
import sys
import time
import tomllib
import urllib.error
import urllib.request

DEFAULT_PREFIX = "/mu/dialogue/v1/peers/"
# Short enough that a dead session stops being addressable promptly, long
# enough that one missed keepalive is survivable.
LEASE_TTL_SECS = 120
HTTP_TIMEOUT_SECS = 5
# `label_max_chars` in [dialogue.presence] is the same setting mu-dialogue's
# presence.rs (in the mu repo) cuts at, with the same default. The file is read
# only LABEL_READ_BYTES far, so a runaway write cannot bloat the key.
DEFAULT_LABEL_MAX_CHARS = 64
LABEL_READ_BYTES = 4096


def log(msg: str) -> None:
    """Diagnostics only when asked; a SessionStart hook must stay silent."""
    if os.environ.get("DIALOGUE_PRESENCE_DEBUG"):
        sys.stderr.write(f"dialogue-presence: {msg}\n")
        sys.stderr.flush()


def config_candidates() -> list[str]:
    """Same precedence presence.rs::config_candidates uses, MU_CONFIG included."""
    override = os.environ.get("MU_CONFIG")
    if override:
        return [override]
    home = os.environ.get("HOME", "/tmp")
    return [
        os.path.join(home, ".config/agent/config.toml"),
        os.path.join(home, ".config/mu/config.toml"),
    ]


def load_presence() -> tuple[list[str], str, int] | None:
    """`[dialogue.presence]` as (endpoints, prefix, label_max_chars), or None
    when not enabled.

    Mirrors presence.rs::load: every "not configured" shape -- no file, no
    section, enabled false, no endpoints -- means run exactly as before.
    """
    for path in config_candidates():
        try:
            with open(path, "rb") as fh:
                root = tomllib.load(fh)
        except (OSError, tomllib.TOMLDecodeError):
            continue
        section = root.get("dialogue", {}).get("presence")
        if not isinstance(section, dict):
            continue
        if not section.get("enabled"):
            return None
        endpoints = [e for e in section.get("etcd", []) if isinstance(e, str) and e]
        if not endpoints:
            return None
        prefix = section.get("prefix") or DEFAULT_PREFIX
        label_max = section.get("label_max_chars", DEFAULT_LABEL_MAX_CHARS)
        if not isinstance(label_max, int) or isinstance(label_max, bool) or label_max < 0:
            log(f"label_max_chars {label_max!r} is not a count; using {DEFAULT_LABEL_MAX_CHARS}")
            label_max = DEFAULT_LABEL_MAX_CHARS
        log(f"presence from {path}: {len(endpoints)} endpoint(s), prefix {prefix}")
        return endpoints, prefix, label_max
    return None


def etcd_post(endpoints: list[str], path: str, payload: dict) -> dict | None:
    """POST to the first etcd endpoint that answers. None when none does."""
    body = json.dumps(payload).encode()
    for ep in endpoints:
        url = f"{ep.rstrip('/')}{path}"
        req = urllib.request.Request(
            url, data=body, headers={"Content-Type": "application/json"}
        )
        try:
            with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_SECS) as resp:
                return json.loads(resp.read() or b"{}")
        except (urllib.error.URLError, OSError, json.JSONDecodeError, TimeoutError) as e:
            log(f"{url}: {e}")
            continue
    return None


def b64(raw: str) -> str:
    return base64.b64encode(raw.encode()).decode()


def label_path(sid: str) -> str:
    return os.path.join(os.environ.get("HOME", "/tmp"), ".cache", f"dialogue-label-{sid}")


def clean_label(raw: str, max_chars: int) -> str | None:
    """Same rule as mu-dialogue's presence.rs::clean_label (mu repo): control characters read as
    spaces, whitespace collapsed, ends trimmed, at most max_chars."""
    spaced = "".join(" " if (ord(c) < 32 or 127 <= ord(c) < 160) else c for c in raw)
    cut = " ".join(spaced.split())[:max_chars].rstrip()
    return cut or None


def read_label(sid: str, max_chars: int) -> str | None:
    """The session's label, or None when there is no usable one.

    A missing, unreadable or empty file all mean "no label"; none of them is a
    reason to stop holding presence. Only a regular file is read, opened
    non-blocking: anyone can put something at that path, and a FIFO there
    would otherwise block the renew loop until the lease lapsed.
    """
    try:
        fd = os.open(label_path(sid), os.O_RDONLY | os.O_NONBLOCK)
    except OSError:
        return None
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            return None
        raw = os.read(fd, LABEL_READ_BYTES)
    except OSError:
        return None
    finally:
        os.close(fd)
    return clean_label(raw.decode("utf-8", errors="replace"), max_chars)


def _ps_field(pid: int, field: str) -> str | None:
    try:
        out = subprocess.run(
            ["ps", "-o", f"{field}=", "-p", str(pid)],
            capture_output=True,
            text=True,
            timeout=HTTP_TIMEOUT_SECS,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return out.stdout.strip() or None


def process_command(pid: int) -> str | None:
    """The pid's command line, or None when `ps` could not answer.

    NOT a liveness test. `ps` is a subprocess that can fail or time out under
    load, and an unanswered question is not a dead process — reading it as one
    is what dropped a live session's presence key twice (2026-10-08/09). Use
    [`is_alive`] to decide whether a process exists; this is only for
    identifying one.
    """
    return _ps_field(pid, "command")


def process_started(pid: int) -> str | None:
    """When the process started, as `ps` reports it.

    The identity check against pid REUSE. Unlike the command line this is not
    something a process can change about itself: Claude Code rewrites its own
    title (`claude … (JSCWarmUp)`), so matching on the title is matching on a
    moving target.
    """
    return _ps_field(pid, "lstart")


def is_alive(pid: int) -> bool:
    """Does `pid` exist? Definitive, and no subprocess involved.

    `os.kill(pid, 0)` sends nothing and reports existence. `ProcessLookupError`
    is a real answer — the process is gone. `PermissionError` means it exists
    and belongs to someone else, which still counts as alive.
    """
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        # Anything else is inconclusive; never conclude death from it.
        return True
    return True


def parent_of(pid: int) -> int:
    ppid = _ps_field(pid, "ppid")
    try:
        return int(ppid or 0)
    except ValueError:
        return 0


def find_claude_ancestor(start: int, max_hops: int = 8) -> int | None:
    """The nearest ancestor that looks like Claude Code, starting at `start`.

    A hook's own parent is not reliably the session process: depending on how
    the host spawns it there may be a shell in between. Rather than guess one
    level, walk up until a command mentions claude. Returning None is a real
    answer — the caller then registers once instead of holding a key it cannot
    honestly keep.
    """
    pid = start
    for _ in range(max_hops):
        if pid <= 1:
            return None
        cmd = process_command(pid)
        if cmd is None:
            return None
        if "claude" in cmd:
            return pid
        pid = parent_of(pid)
    return None


def main() -> int:
    ap = argparse.ArgumentParser(add_help=True)
    ap.add_argument("--session-id", default=os.environ.get("CLAUDE_CODE_SESSION_ID", ""))
    ap.add_argument(
        "--watch-pid",
        type=int,
        default=0,
        help="the Claude Code process to follow; the lease is dropped when it exits",
    )
    ap.add_argument("--ttl", type=int, default=LEASE_TTL_SECS)
    ap.add_argument(
        "--once",
        action="store_true",
        help="register and exit without holding (for tests and for an unverifiable pid)",
    )
    args = ap.parse_args()

    sid = args.session_id.strip()
    if not sid:
        log("no session id")
        return 0

    # Single instance per session, the same guard dialogue-rewake.sh uses:
    # SessionStart fires again on resume and on /clear, and two holders would
    # race to own one key — the loser renewing a lease that no longer carries
    # it, so the key would vanish under a live session.
    lock_path = os.path.join(
        os.environ.get("HOME", "/tmp"), ".cache", f"dialogue-presence-{sid}.lock"
    )
    if not args.once:
        os.makedirs(os.path.dirname(lock_path), exist_ok=True)
        try:
            with open(lock_path) as fh:
                other = int(fh.read().strip() or 0)
            os.kill(other, 0)
        except (OSError, ValueError):
            pass  # no lock, unreadable, or the holder is gone — ours to take
        else:
            log(f"holder {other} already owns this session")
            return 0
        with open(lock_path, "w") as fh:
            fh.write(str(os.getpid()))

    presence = load_presence()
    if presence is None:
        log("presence not enabled; leaving activity-derived presence alone")
        return 0
    endpoints, prefix, label_max = presence

    peer_id = f"cc:{sid}"
    key = f"{prefix}{peer_id}"

    # A keeper that cannot tell when the session dies would hold the key
    # forever, which is the exact lie this exists to remove. Take the pid if it
    # is recognisably Claude Code, else look for it among our ancestors, else
    # register once and stop.
    watch_pid = 0
    if args.watch_pid > 0:
        cmd = process_command(args.watch_pid)
        if cmd and "claude" in cmd:
            watch_pid = args.watch_pid
        else:
            log(f"pid {args.watch_pid} is not Claude Code ({cmd!r}); searching ancestors")
    if watch_pid == 0:
        found = find_claude_ancestor(args.watch_pid or os.getppid())
        if found:
            watch_pid = found
            log(f"following ancestor pid {watch_pid}")
    verified = watch_pid > 0
    if not verified:
        log("no Claude Code process to follow; will register without holding")
    # Pin the identity ONCE, by start time, so pid reuse is caught without
    # re-reading a title the process rewrites as it works.
    watch_started = process_started(watch_pid) if verified else None

    granted = etcd_post(endpoints, "/v3/lease/grant", {"TTL": str(args.ttl)})
    lease_id = (granted or {}).get("ID")
    if not lease_id:
        log("lease grant failed; falling back to activity-derived presence")
        return 0

    registered_at = int(time.time() * 1000)

    def value_for(label: str | None) -> str:
        fields = {
            "peer_id": peer_id,
            "role": "cc",
            "registered_at_unix_ms": registered_at,
            "host": os.uname().nodename,
            "pid": os.getpid(),
        }
        if label is not None:
            fields["label"] = label
        return json.dumps(fields)

    def put_key(lease: str, label: str | None) -> dict | None:
        return etcd_post(
            endpoints,
            "/v3/kv/put",
            {"key": b64(key), "value": b64(value_for(label)), "lease": str(lease)},
        )

    label = read_label(sid, label_max)
    put = put_key(lease_id, label)
    if put is None:
        log("put failed; revoking the lease so nothing half-registered is left")
        etcd_post(endpoints, "/v3/lease/revoke", {"ID": str(lease_id)})
        return 0
    log(f"registered {key} on lease {lease_id} (ttl {args.ttl}s)")

    def drop(*_: object) -> None:
        """Revoke so the key clears now rather than at TTL."""
        etcd_post(endpoints, "/v3/lease/revoke", {"ID": str(lease_id)})
        log("lease revoked")
        if not args.once:
            try:
                os.unlink(lock_path)
            except OSError:
                pass
        sys.exit(0)

    signal.signal(signal.SIGTERM, drop)
    signal.signal(signal.SIGINT, drop)

    if args.once or not verified:
        # Registered, but with nothing trustworthy to follow. Let the TTL end
        # it rather than hold a key no one is behind.
        log("registered without holding; the lease will expire on its own")
        return 0

    # Renew well inside the TTL so one lost request is not fatal.
    interval = max(5, args.ttl // 3)

    while True:
        time.sleep(interval)
        # Existence first, and only a definitive answer ends the hold. The
        # earlier version asked `ps` for the command line and treated a blank
        # reply as death, so one failed subprocess dropped a live session's
        # key — twice, observed.
        if not is_alive(watch_pid):
            log(f"watched pid {watch_pid} no longer exists; dropping the lease")
            drop()
        # Then identity, against pid reuse. A start time that CHANGED means a
        # different process now holds the number; a start time we simply could
        # not read means nothing and is ignored.
        if watch_started is not None:
            now_started = process_started(watch_pid)
            if now_started is not None and now_started != watch_started:
                log(f"pid {watch_pid} was reused by another process; dropping the lease")
                drop()
        alive = etcd_post(endpoints, "/v3/lease/keepalive", {"ID": str(lease_id)})
        # etcd answers a keepalive for an expired lease with a zero TTL. Treat
        # that as the registration being gone and put it back, rather than
        # renewing nothing for the rest of the session.
        ttl_left = ((alive or {}).get("result") or {}).get("TTL")
        if alive is None:
            log("keepalive unreachable; will try again next cycle")
            continue
        if ttl_left in (None, "0", 0, "-1", -1):
            log("lease had expired; re-registering")
            granted = etcd_post(endpoints, "/v3/lease/grant", {"TTL": str(args.ttl)})
            new_id = (granted or {}).get("ID")
            if not new_id:
                continue
            new_label = read_label(sid, label_max)
            if put_key(new_id, new_label) is None:
                # Keep nothing from this attempt. lease_id stays the expired
                # lease, so the next keepalive fails and this branch runs
                # again; the new lease is revoked rather than left to renew
                # with no key on it.
                etcd_post(endpoints, "/v3/lease/revoke", {"ID": str(new_id)})
                log("re-register put failed; will retry next cycle")
                continue
            lease_id = new_id
            label = new_label
            continue
        # The lease is fine; carry a changed label onto it. Same lease, so the
        # key never lapses. On a failed put `label` keeps its old value and
        # the next cycle tries again.
        now_label = read_label(sid, label_max)
        if now_label != label and put_key(lease_id, now_label) is not None:
            log(f"label now {now_label!r}")
            label = now_label


if __name__ == "__main__":
    sys.exit(main())
