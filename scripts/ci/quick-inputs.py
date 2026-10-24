#!/usr/bin/env python3
"""Keep checkout timestamps for byte-identical inputs in the Windows Quick cache.

Cargo still checks fingerprints normally. Changed/new files keep their checkout
timestamps; removed files are absent. No generated files or secrets are cached
here, only hashes and timestamps of tracked build inputs. Directory timestamps
are deliberately left alone (Cargo may conservatively rerun a build script).
"""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    mode, root_arg, state_arg = sys.argv[1:]
    root = Path(root_arg).resolve()
    state = Path(state_arg)
    previous = json.loads(state.read_text()) if state.exists() else {}
    tracked = subprocess.check_output(["git", "ls-files", "-z"], cwd=root)
    current = {}
    restored = 0
    for name in tracked.decode("utf-8").split("\0"):
        if not name:
            continue
        path = root / name
        if path.is_symlink() or not path.is_file():
            continue
        path.resolve().relative_to(root)
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        stat = path.stat()
        old = previous.get(name)
        if mode == "restore" and old and old[0] == digest:
            os.utime(path, ns=(stat.st_atime_ns, old[1]))
            restored += 1
        current[name] = [digest, path.stat().st_mtime_ns]
    if mode == "save":
        state.parent.mkdir(parents=True, exist_ok=True)
        state.write_text(json.dumps(current), encoding="utf-8")
    elif mode != "restore":
        raise ValueError("expected restore or save")
    print(f"Quick inputs: {len(current)} tracked files, {restored} timestamps restored")


if __name__ == "__main__":
    main()
