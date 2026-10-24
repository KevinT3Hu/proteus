# Read-only remote workspace discovery. Requires Python 3, no project code imported.
import json
import os
import stat
import sys

LIMIT = 262144


def read(path, optional=False):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
    except FileNotFoundError:
        if optional:
            return None
        raise
    with os.fdopen(fd, "rb") as f:
        if not stat.S_ISREG(os.fstat(f.fileno()).st_mode):
            raise ValueError("expected regular file: " + path)
        data = f.read(LIMIT + 1)
        if len(data) > LIMIT:
            raise ValueError("workspace file exceeds 256 KiB")
        return data.decode("utf-8")


root = os.path.realpath(sys.argv[1] or os.path.expanduser("~"))
if not os.path.isdir(root):
    raise ValueError("remote working directory does not exist")
while True:
    path = os.path.join(root, ".proteus-workspace")
    # lexists makes a malformed nearest definition fail rather than fall through.
    if os.path.lexists(path):
        print(
            json.dumps(
                {
                    "root": root,
                    "definition": read(path),
                    "package": read(os.path.join(root, "package.json"), True),
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
