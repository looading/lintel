#!/usr/bin/env python3
"""Prepare real package resources without Cargo, Git, downloads or tar extraction."""

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import stat
import tarfile
import tempfile


PACKAGE = Path(__file__).resolve().parents[1]
ROOT = PACKAGE.parents[1]
BUILD_RESOURCES = ROOT / "crates/lintel-build/resources"
MAX_ARCHIVE_BYTES = 9_750_000
MAX_MEMBER_BYTES = 32 * 1024 * 1024
MAX_UNPACKED_BYTES = 128 * 1024 * 1024
SOURCE_SUFFIXES = {".rs", ".wgsl", ".svg", ".js", ".html"}
ARCHIVE = "lintel-framework.tar.gz"
CHECKSUM = "lintel-framework.sha256"


def relative_path(value):
    path = PurePosixPath(value)
    if not value or path.is_absolute() or "\\" in value or any(p in {"", ".", ".."} for p in value.split("/")):
        raise ValueError("resource path must be a plain relative path")
    for component in path.parts:
        lower = component.lower()
        if lower in {".git", ".gitconfig", ".gitmodules", ".env", ".netrc", ".npmrc", ".git-credentials", "id_rsa", "id_ed25519"} or lower.startswith(".env."):
            raise ValueError("resource path contains repository metadata or a credential container")
    return Path(*path.parts)


def real_file(path):
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError(f"resource must be a regular file: {path.name}")
    return path


def read_map(path):
    value = json.loads(real_file(path).read_text())
    if value.get("schema_version") != 1:
        raise ValueError("unsupported resource/source recipe version")
    return value


def material_pairs():
    mapping = read_map(BUILD_RESOURCES / "source-map.json")
    pairs = [(ROOT / relative_path(name), BUILD_RESOURCES / relative_path(name)) for name in mapping["files"]]
    pairs += [
        (ROOT / "LICENSE", ROOT / "crates/lintel-build/LICENSE"),
        (ROOT / "LICENSE", PACKAGE / "resources/LICENSE"),
        (ROOT / "assets/svg/close.svg", ROOT / "crates/lintel-iced-widgets/assets/svg/close.svg"),
    ]
    return pairs


def sync_materials():
    pairs = material_pairs()
    for source, target in pairs:
        content = real_file(source).read_bytes()
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(content)
    return len(pairs)


def check_materials():
    pairs = material_pairs()
    for source, target in pairs:
        if real_file(source).read_bytes() != real_file(target).read_bytes():
            raise ValueError(f"stale package resource: {target.relative_to(ROOT)}; run --sync")
    return len(pairs)


def expected_sources():
    recipe = read_map(PACKAGE / "resources/framework-sources.json")
    expected = {}

    def add(relative):
        relative = relative_path(str(relative).replace(os.sep, "/"))
        real_file(ROOT / relative)
        expected[relative.as_posix()] = ROOT / relative

    def tree(relative, framework):
        base = ROOT / relative
        if not stat.S_ISDIR(base.lstat().st_mode):
            raise ValueError(f"source tree must be a real directory: {relative}")
        for entry in sorted(base.iterdir()):
            name = entry.name
            destination = relative / name
            relative_path(destination.as_posix())
            if name in {"target", "build"} or (framework and (name.startswith(".") or name == "third_party")):
                continue
            mode = entry.lstat().st_mode
            if stat.S_ISDIR(mode):
                tree(destination, framework)
            elif stat.S_ISREG(mode):
                if not framework or entry.suffix in SOURCE_SUFFIXES:
                    add(destination)
            else:
                raise ValueError(f"source tree has a special file: {destination}")

    for name in ["Cargo.toml", "Cargo.lock", ".cargo/macos-public-dependencies.toml", "profiles/macos-public/Cargo.lock"] + recipe["files"]:
        add(relative_path(name))
    for name in recipe["crates"]:
        directory = Path("crates") / relative_path(name)
        add(directory / "Cargo.toml")
        tree(directory / "src", True)
        for notice in ["LICENSE", "README.md"]:
            if (ROOT / directory / notice).is_file():
                add(directory / notice)
    for name in ["src", "assets", "crates/lintel-iced-widgets/assets"]:
        tree(relative_path(name), True)
    for name in recipe["resource_trees"]:
        tree(relative_path(name), False)
    tree(Path("vendor/iced"), False)
    return expected


def verify_archive(path):
    path = real_file(path)
    size = path.stat().st_size
    if not 0 < size <= MAX_ARCHIVE_BYTES:
        raise ValueError("framework archive exceeds its registry package size budget")
    expected = expected_sources()
    seen = set()
    expanded = 0
    with gzip.open(path, "rb") as compressed:
        with tarfile.open(fileobj=compressed, mode="r|") as archive:
            for member in archive:
                name = relative_path(member.name).as_posix()
                if not member.isfile() or name in seen or name not in expected:
                    raise ValueError(f"unexpected, duplicate or special framework member: {name}")
                seen.add(name)
                expanded += member.size
                if member.size > MAX_MEMBER_BYTES or expanded > MAX_UNPACKED_BYTES or len(seen) > 10_000:
                    raise ValueError("framework member/source size budget exceeded")
                content = archive.extractfile(member).read(MAX_MEMBER_BYTES + 1)
                if len(content) != member.size or content != real_file(expected[name]).read_bytes():
                    raise ValueError(f"stale or mismatched framework member: {name}")
        # Validate the gzip trailer even after tar's end marker has been consumed.
        while compressed.read(1024 * 1024):
            pass
    if seen != set(expected):
        missing = sorted(set(expected) - seen)
        raise ValueError(f"framework archive is incomplete; first missing member: {missing[0]}")
    checksum = hashlib.sha256(path.read_bytes()).hexdigest()
    return {"framework_members": len(seen), "archive_bytes": size, "sha256": checksum}


def atomic_write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=".lintel-resource-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(content)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sync", action="store_true", help="sync small canonical resources and real MIT notices")
    parser.add_argument("--archive", type=Path, help="fresh OUT_DIR framework archive to verify and prepare")
    parser.add_argument("--output-dir", type=Path, default=PACKAGE / "resources", help="explicit prepared asset directory for registry-stage overlays")
    parser.add_argument("--check", action="store_true", help="verify small resources and already prepared archive/checksum")
    arguments = parser.parse_args()
    if not (arguments.sync or arguments.archive or arguments.check):
        parser.error("choose --sync, --archive PATH, or --check")
    if not (ROOT / "Cargo.toml").is_file() or not (ROOT / "vendor/iced/Cargo.toml").is_file():
        raise ValueError("release preparation requires the complete source checkout")
    result = {}
    if arguments.sync:
        result["resource_files_synced"] = sync_materials()
    result["resource_files_checked"] = check_materials()
    if arguments.archive:
        result.update(verify_archive(arguments.archive))
        content = arguments.archive.read_bytes()
        if hashlib.sha256(content).hexdigest() != result["sha256"]:
            raise ValueError("framework archive changed during preparation")
        atomic_write(arguments.output_dir / ARCHIVE, content)
        atomic_write(arguments.output_dir / CHECKSUM, (result["sha256"] + "\n").encode("ascii"))
    if arguments.check:
        result.update(verify_archive(arguments.output_dir / ARCHIVE))
        checksum = real_file(arguments.output_dir / CHECKSUM).read_text().strip()
        if checksum != result["sha256"]:
            raise ValueError("prepared framework archive SHA-256 mismatch")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, tarfile.TarError) as error:
        raise SystemExit(str(error)) from None
