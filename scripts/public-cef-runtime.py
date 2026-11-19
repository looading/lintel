#!/usr/bin/env python3
"""Locate the verified macOS CEF distribution selected by Cargo."""

import argparse
import json
from pathlib import Path
import subprocess
import sys


VERSION = "143.0.14+gdd46a37+chromium-143.0.7499.193"
ARCHIVES = {
    "aarch64": ("macosarm64", "8ed061dc1196d8118019af35f0c6af4eeac9da6f", "arm64"),
    "x86_64": ("macosx64", "a2eabe2361305bb1cd4032db65c4c476bfbd9fe5", "x86_64"),
}
FRAMEWORK = "Chromium Embedded Framework"


def validate_runtime(path, arch):
    path = path.resolve(strict=True)
    platform, sha1, architecture = ARCHIVES[arch]
    archive_name = f"cef_binary_{VERSION}_{platform}_minimal.tar.bz2"
    metadata = json.loads((path / "archive.json").read_text())
    expected = {"type": "minimal", "name": archive_name, "sha1": sha1}
    if not isinstance(metadata, dict) or any(metadata.get(key) != value for key, value in expected.items()):
        raise ValueError(f"expected the official {archive_name} at {path}")

    header = (path / "include/cef_version.h").read_text().splitlines()
    if f'#define CEF_VERSION "{VERSION}"' not in header:
        raise ValueError(f"CEF headers at {path} do not match {VERSION}")

    framework = path / f"{FRAMEWORK}.framework"
    binary = framework / FRAMEWORK
    sandbox = framework / "Libraries/libcef_sandbox.dylib"
    for library in (binary, sandbox):
        if not library.is_file():
            raise ValueError(f"missing CEF binary: {library}")
        subprocess.run(["lipo", "-verify_arch", architecture, str(library)], check=True)
    return path


def built_artifacts(log, arch):
    messages = [json.loads(line) for line in log.read_text().splitlines() if line.startswith("{")]
    cef_packages = set()
    executables = set()
    for message in messages:
        if message.get("reason") != "compiler-artifact":
            continue
        target = message["target"]
        if target["name"] == "cef_dll_sys":
            cef_packages.add(message["package_id"])
        if target["name"] == "webview" and "example" in target["kind"] and message.get("executable"):
            executables.add(Path(message["executable"]))

    runtimes = set()
    for message in messages:
        if message.get("reason") != "build-script-executed" or message.get("package_id") not in cef_packages:
            continue
        for link in message["linked_paths"]:
            if link.startswith("native="):
                path = Path(link.removeprefix("native="))
                if (path / "archive.json").is_file() and (path / f"{FRAMEWORK}.framework").is_dir():
                    runtimes.add(validate_runtime(path, arch))
    if len(runtimes) != 1 or len(executables) != 1:
        raise ValueError(f"expected one CEF distribution and one WebView executable in Cargo output {log}")
    return runtimes.pop(), executables.pop()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=ARCHIVES, required=True)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--path", type=Path)
    mode.add_argument("--build-log", type=Path)
    args = parser.parse_args()
    try:
        if args.path is not None:
            print(validate_runtime(args.path, args.arch))
        else:
            runtime, executable = built_artifacts(args.build_log, args.arch)
            print(runtime)
            print(executable)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Public CEF runtime: {error}\n")


if __name__ == "__main__":
    main()
