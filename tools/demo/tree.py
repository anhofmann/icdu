#!/usr/bin/env python3
"""Build the sample tree the README's demo is recorded on.

    tree.py DIR

The tree looks like a home directory: projects with their build output and dependencies,
photos, videos, caches. It is the same every time (the names and sizes come from a fixed
seed), so the recording can be made again when the interface changes.

Large files get their size from posix_fallocate: the blocks are reserved, which is what a
disk usage tool reports, and nothing is written. The tree takes about 4 GB of disk space
and 53,000 entries while it exists. Remove it afterwards.
"""
import os
import random
import sys

MIB = 1 << 20


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    root = sys.argv[1]
    if os.path.exists(root):
        sys.exit(f"{root} exists already: give a path that does not")
    rng = random.Random(2026)

    def file(path, size):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
        try:
            if size >= 64 * 1024:
                os.posix_fallocate(fd, 0, size)
            elif size:
                os.write(fd, b"\0" * size)
        finally:
            os.close(fd)

    def mixed(i, bits):
        """A number that looks arbitrary and differs for every `i`: for file names."""
        return (i * 2654435761 + 12345) % (1 << bits)

    def many(directory, count, name, low, high):
        for i in range(count):
            file(os.path.join(directory, name(i)), rng.randint(low, high))

    home = os.path.join(root, "home")

    # Projects: sources are small, what is built from them and fetched for them is not.
    shop = os.path.join(home, "projects", "webshop")
    packages = [
        "react", "lodash", "webpack", "typescript", "eslint", "babel-core", "rxjs", "express",
        "moment", "chalk", "axios", "jest", "prettier", "esbuild", "postcss", "sass",
    ]
    for n in range(900):
        package = os.path.join(shop, "node_modules", f"{rng.choice(packages)}-{n:03}")
        many(os.path.join(package, "lib"), rng.randint(8, 30), lambda i: f"index{i}.js", 400, 30_000)
        many(os.path.join(package, "dist"), rng.randint(2, 6), lambda i: f"bundle{i}.min.js", 20_000, 150_000)
        file(os.path.join(package, "package.json"), 900)
    many(os.path.join(shop, "src", "components"), 140, lambda i: f"Component{i}.tsx", 800, 9_000)
    many(os.path.join(shop, "dist", "assets"), 40, lambda i: f"chunk-{i:02}.js", 100_000, 2 * MIB)
    many(os.path.join(shop, ".git", "objects", "pack"), 6, lambda i: f"pack-{i}.pack", 8 * MIB, 30 * MIB)

    compiler = os.path.join(home, "projects", "compiler")
    many(os.path.join(compiler, "src"), 180, lambda i: f"pass_{i:03}.rs", 2_000, 40_000)
    for profile, files, low, high in [("debug", 300, 200_000, 2 * MIB), ("release", 160, 100_000, MIB)]:
        many(os.path.join(compiler, "target", profile, "deps"), files, lambda i: f"lib{i:03}.rlib", low, high)
        many(os.path.join(compiler, "target", profile, "incremental"), 2500, lambda i: f"s-{i:05}.bin", 1_000, 30_000)
    many(os.path.join(compiler, ".git", "objects", "pack"), 3, lambda i: f"pack-{i}.pack", 5 * MIB, 40 * MIB)
    many(os.path.join(home, "projects", "notes"), 60, lambda i: f"note-{i:03}.md", 300, 12_000)

    # Pictures and films: few entries, most of the space.
    for year in range(2021, 2026):
        for month in rng.sample(range(1, 13), 4):
            first = rng.randint(1000, 9000)
            many(
                os.path.join(home, "photos", str(year), f"{month:02}"),
                rng.randint(8, 22),
                lambda i: f"IMG_{first + 3 * i}.jpg",
                1 * MIB,
                4 * MIB,
            )
    for name, size in [("holiday-2024.mp4", 410), ("wedding.mkv", 280), ("talk-recording.mp4", 150), ("timelapse.mov", 60)]:
        file(os.path.join(home, "videos", name), size * MIB)
    for name, size in [("debian-13.iso", 190), ("dataset.tar.zst", 120), ("backup-old.zip", 70), ("manual.pdf", 9)]:
        file(os.path.join(home, "downloads", name), size * MIB)
    many(os.path.join(home, "music", "albums"), 90, lambda i: f"track-{i:02}.flac", 12 * MIB // 10, 4 * MIB)

    # Caches: very many small files, in several places. This is what the demo searches for.
    for n in range(260):
        many(os.path.join(home, ".cache", "browser", "Cache_Data", f"{n:02x}"), 60, lambda i: f"f_{mixed(i, 24):06x}", 500, 40_000)
    many(os.path.join(home, ".cache", "thumbnails", "large"), 2400, lambda i: f"{mixed(i, 40):010x}.png", 4_000, 60_000)
    many(os.path.join(home, ".cache", "pip", "http"), 1500, lambda i: f"{mixed(i + 7, 40):010x}", 2_000, 100_000)
    many(os.path.join(shop, "node_modules", ".cache", "babel-loader"), 1800, lambda i: f"{mixed(i + 3, 40):010x}.json", 2_000, 200_000)
    many(os.path.join(compiler, "target", "debug", "build", "cache"), 900, lambda i: f"out-{i:04}.o", 10_000, 500_000)
    many(os.path.join(home, ".local", "share", "app", "logs"), 300, lambda i: f"app.{i}.log", 10_000, 900_000)
    many(os.path.join(home, ".config", "app"), 25, lambda i: f"settings-{i}.toml", 200, 4_000)

    entries = sum(len(dirs) + len(files) for _, dirs, files in os.walk(root))
    print(f"{home}: {entries} entries")


if __name__ == "__main__":
    main()
