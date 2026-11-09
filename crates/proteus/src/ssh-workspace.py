# Read-only remote workspace discovery. Requires Python 3, no project code imported.
import json
import os
import stat
import sys

LIMIT = 262144
EUID = os.geteuid()
PRIVATE = []


def number(s):
    try:
        return int(s)
    except ValueError:
        return None


def private_group():
    # Mirrors proteus_pty::secure_fs: named after the user, no other members,
    # and no other user's primary group.
    if not PRIVATE:
        PRIVATE.append(None)
        try:
            with open("/etc/passwd") as f:
                users = [l.split(":") for l in f.read().splitlines()]
            with open("/etc/group") as f:
                groups = [l.split(":") for l in f.read().splitlines()]
        except (OSError, UnicodeDecodeError):
            return None
        users = [u for u in users if len(u) >= 4]
        me = [u for u in users if number(u[2]) == EUID]
        if not me or number(me[0][3]) is None:
            return None
        name, gid = me[0][0], number(me[0][3])
        if any(u[0] != name and number(u[3]) == gid for u in users):
            return None
        entries = [g for g in groups if len(g) >= 4 and number(g[2]) == gid]
        if (
            len(entries) == 1
            and entries[0][0] == name
            and all(m in ("", name) for m in entries[0][3].split(","))
        ):
            PRIVATE[0] = gid
    return PRIVATE[0]


def unshared(st):
    return (
        st.st_uid in (EUID, 0)
        and not st.st_mode & 0o002
        and (not st.st_mode & 0o020 or (st.st_uid == EUID and st.st_gid == private_group()))
    )


def contents(f, path):
    if not stat.S_ISREG(os.fstat(f.fileno()).st_mode):
        raise ValueError("expected regular file: " + path)
    data = f.read(LIMIT + 1)
    if len(data) > LIMIT:
        raise ValueError("workspace file exceeds 256 KiB")
    return data.decode("utf-8")


def open_nofollow(path):
    return os.fdopen(os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW), "rb")


def read_optional(path):
    try:
        f = open_nofollow(path)
    except FileNotFoundError:
        return None
    with f:
        return contents(f, path)


root = os.path.realpath(sys.argv[1] or os.path.expanduser("~"))
if not os.path.isdir(root):
    raise ValueError("remote working directory does not exist")
while True:
    path = os.path.join(root, ".proteus-workspace")
    # A symlink fails rather than falls through; untrusted files are skipped.
    try:
        f = open_nofollow(path)
    except FileNotFoundError:
        f = None
    if f is not None:
        with f:
            st, dir_st = os.fstat(f.fileno()), os.stat(root)
            dir_safe = unshared(dir_st) or (dir_st.st_uid in (EUID, 0) and dir_st.st_mode & stat.S_ISVTX)
            if unshared(st) and dir_safe:
                print(
                    json.dumps(
                        {
                            "root": root,
                            "definition": contents(f, path),
                            "package": read_optional(os.path.join(root, "package.json")),
                            "locks": [
                                p
                                for p in (
                                    "pnpm-lock.yaml",
                                    "yarn.lock",
                                    "bun.lock",
                                    "bun.lockb",
                                )
                                if os.path.isfile(os.path.join(root, p))
                            ],
                        }
                    )
                )
                break
    parent = os.path.dirname(root)
    if parent == root:
        print("null")
        break
    root = parent
