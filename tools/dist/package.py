#!/usr/bin/env python3
"""Pack a release binary into the archive that gets published.

    tools/dist/package.py TARGET BINARY OUT_DIR

Writes OUT_DIR/icdu-VERSION-TARGET.tar.gz (the version is the one in Cargo.toml) and prints
its path. The archive holds one directory, icdu-VERSION-TARGET, with:

    icdu          the binary
    LICENSE       the licence asks for its text to go with every copy
    README.md
    CHANGELOG.md

The same input gives the same archive, byte for byte, on Linux and on macOS: entries are in
a fixed order, belong to root, and carry the time of the commit they were built from
(SOURCE_DATE_EPOCH, or else the time of the last commit).
"""
import gzip
import io
import os
import re
import subprocess
import sys
import tarfile


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    target, binary, out_dir = sys.argv[1:]
    top = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

    with open(os.path.join(top, "Cargo.toml"), encoding="utf-8") as f:
        version = re.search(r'^version = "([^"]+)"$', f.read(), re.M).group(1)
    epoch = os.environ.get("SOURCE_DATE_EPOCH") or subprocess.run(
        ["git", "-C", top, "log", "-1", "--format=%ct"], capture_output=True, text=True, check=True
    ).stdout.strip()

    name = f"icdu-{version}-{target}"
    files = [(binary, "icdu", 0o755)]
    files += [(os.path.join(top, doc), doc, 0o644) for doc in ("LICENSE", "README.md", "CHANGELOG.md")]

    os.makedirs(out_dir, exist_ok=True)
    path = os.path.join(out_dir, name + ".tar.gz")
    with open(path, "wb") as raw:
        # No file name and no time in the gzip header either.
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as packed:
            with tarfile.open(fileobj=packed, mode="w", format=tarfile.PAX_FORMAT) as tar:

                def entry(arcname, mode, kind, size=0):
                    info = tarfile.TarInfo(arcname)
                    info.type, info.mode, info.size, info.mtime = kind, mode, size, int(epoch)
                    info.uid = info.gid = 0
                    info.uname = info.gname = "root"
                    return info

                tar.addfile(entry(name, 0o755, tarfile.DIRTYPE))
                for source, arcname, mode in files:
                    with open(source, "rb") as f:
                        data = f.read()
                    tar.addfile(entry(f"{name}/{arcname}", mode, tarfile.REGTYPE, len(data)), io.BytesIO(data))
    print(path)


if __name__ == "__main__":
    main()
