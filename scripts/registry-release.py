#!/usr/bin/env python3
"""Create an external, registry-only release workspace without invoking other tools."""

from __future__ import annotations

import argparse
import copy
import dataclasses
import datetime
import hashlib
import json
import math
from pathlib import Path
import re
import shutil
import struct
import sys
import tempfile
import tomllib
from typing import Any


SCHEMA_VERSION = 1
PINS = {
    "wgpu": "a6d9d3254e8d5ed7de08634cd9ddd7b63956f7f3",
    "winit": "48116469f15d3bc52b5e56bb516694be8882870d",
    "cef": "348fef293fd48b053b6157cecc3e05c36d71d0ca",
    "cryoglyph": "3836ca7a17f410d30871c2254dcc0f2400876636",
}
ICED_REVISION = "90037336e80f02b817f2f68a5adf877bdb85639a"
WGPU_PACKAGES = (
    "wgpu", "wgpu-core", "wgpu-hal", "wgpu-core-deps-apple",
    "wgpu-core-deps-windows-linux-android", "wgpu-core-deps-emscripten",
    "wgpu-core-deps-wasm",
)
SKIP_DIRECTORIES = {".git", ".ai", "target", "build", "third_party", "node_modules", "__pycache__"}
SKIP_FILES = {".cargo-ok", ".cargo_vcs_info.json", "Cargo.toml.orig", "Cargo.lock", ".DS_Store"}
DEPENDENCY_SECTIONS = ("dependencies", "build-dependencies", "dev-dependencies")


class ReleaseError(Exception):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ReleaseError(message)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def read_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text())
    require(isinstance(value, dict), f"expected a JSON object: {path.name}")
    return value


def read_toml(path: Path) -> dict[str, Any]:
    return tomllib.loads(path.read_text())


def quoted(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def toml_value(value: Any) -> str:
    if isinstance(value, str):
        return quoted(value)
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        require(math.isfinite(value), "non-finite TOML metadata is unsupported")
        return repr(value)
    if isinstance(value, (datetime.datetime, datetime.date, datetime.time)):
        return value.isoformat()
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        return "{ " + ", ".join(f"{quoted(key)} = {toml_value(item)}" for key, item in value.items()) + " }"
    raise ReleaseError(f"unsupported TOML value type: {type(value).__name__}")


def render_toml(document: dict[str, Any]) -> str:
    lines: list[str] = []

    def table(value: dict[str, Any], path: tuple[str, ...], header: str | None = None) -> None:
        if header:
            lines.extend(["", header])
        for key, item in value.items():
            if not isinstance(item, dict) and not (isinstance(item, list) and item and all(isinstance(x, dict) for x in item)):
                lines.append(f"{quoted(key)} = {toml_value(item)}")
        for key, item in value.items():
            child = (*path, key)
            name = ".".join(quoted(part) for part in child)
            if isinstance(item, dict):
                table(item, child, f"[{name}]")
            elif isinstance(item, list) and item and all(isinstance(x, dict) for x in item):
                for entry in item:
                    table(entry, child, f"[[{name}]]")

    table(document, ())
    output = "\n".join(lines).lstrip() + "\n"
    require(tomllib.loads(output) == document, "TOML roundtrip changed manifest semantics")
    return output


def safe_relative(value: str) -> Path:
    path = Path(value)
    require(not path.is_absolute() and path.parts and ".." not in path.parts and "\\" not in value,
            "resource destination must be a plain relative package path")
    require(all(part.lower() != ".git" for part in path.parts), "Git metadata is not a release resource")
    return path


def safe_package_name(value: str) -> str:
    require(bool(re.fullmatch(r"[a-z][a-z0-9_-]{0,63}", value)), "invalid registry package name")
    return value


def workspace_for(manifest: Path, document: dict[str, Any]) -> tuple[Path, dict[str, Any]]:
    if "workspace" in document:
        return manifest.parent, document["workspace"]
    explicit = document.get("package", {}).get("workspace")
    if explicit:
        path = (manifest.parent / explicit).resolve() / "Cargo.toml"
        return path.parent, read_toml(path).get("workspace", {})
    for parent in manifest.parent.parents:
        candidate = parent / "Cargo.toml"
        if candidate.is_file():
            data = read_toml(candidate)
            if "workspace" in data:
                return parent, data["workspace"]
    return manifest.parent, {}


class GitInput:
    """Check the selected cached revision and every copied tracked blob, without Git."""

    def __init__(self, root: Path, revision: str):
        self.root = root
        metadata = root / ".git"
        if metadata.is_file():
            pointer = metadata.read_text().strip()
            require(pointer.startswith("gitdir: "), "invalid cached Git metadata pointer")
            metadata = (root / pointer[8:]).resolve()
        require(metadata.is_dir(), "pinned source requires an existing cached Git checkout")
        common = metadata
        if (metadata / "commondir").is_file():
            common = (metadata / (metadata / "commondir").read_text().strip()).resolve()
        head = (metadata / "HEAD").read_text().strip()
        if head.startswith("ref: "):
            reference = head[5:]
            found = next((p for p in [metadata / reference, common / reference] if p.is_file()), None)
            if found:
                head = found.read_text().strip()
            else:
                entries = (common / "packed-refs").read_text().splitlines()
                head = next((line.split()[0] for line in entries if line.endswith(" " + reference)), "")
        require(head == revision, "cached source revision does not match the explicit release pin")
        index = (metadata / "index").read_bytes()
        require(index[:4] == b"DIRC", "cached source has no readable Git index")
        version, count = struct.unpack_from(">II", index, 4)
        require(version in (2, 3), "cached Git index version is unsupported; export a verified source snapshot first")
        self.blobs: dict[str, bytes] = {}
        offset = 12
        for _ in range(count):
            start = offset
            flags = struct.unpack_from(">H", index, start + 60)[0]
            require(not flags & 0x3000, "cached source index contains an unresolved merge")
            blob = index[start + 40:start + 60]
            offset = start + 62 + (2 if version == 3 and flags & 0x4000 else 0)
            end = index.index(b"\0", offset)
            name = index[offset:end].decode("utf-8")
            offset = start + ((end + 1 - start + 7) // 8) * 8
            self.blobs[name] = blob

    def verify(self, path: Path, data: bytes) -> None:
        relative = path.resolve().relative_to(self.root).as_posix()
        expected = self.blobs.get(relative)
        require(expected is not None, f"untracked file in pinned source: {relative}")
        actual = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).digest()
        require(actual == expected, f"modified file in pinned source: {relative}")


@dataclasses.dataclass
class Package:
    original: str
    name: str
    version: str
    manifest: Path
    workspace_root: Path
    workspace: dict[str, Any]
    document: dict[str, Any]
    group: str
    verifier: GitInput | None = None
    revision: str | None = None
    transformed: dict[str, Any] | None = None
    dependencies: set[str] = dataclasses.field(default_factory=set)
    omissions: list[str] = dataclasses.field(default_factory=list)


class Stager:
    def __init__(self, root: Path, settings: dict[str, Any], resources: dict[str, Any], excluded: set[str]):
        require(settings.get("schema_version") == 1, "unsupported source-map schema_version")
        require(not resources or resources.get("schema_version") == 1, "unsupported resources manifest schema_version")
        self.root = root
        self.settings = settings
        self.resources = resources.get("packages", {})
        self.excluded = excluded
        self.packages: dict[str, Package] = {}
        self.by_manifest: dict[Path, Package] = {}
        self.overrides = settings.get("registry_overrides", {})
        for original, override in self.overrides.items():
            require(isinstance(override, dict) and override.get("version"), f"registry override needs a version: {original}")
            require(str(override["version"]).startswith("="), f"registry override must use an exact version: {original}")
        self.inputs: list[dict[str, Any]] = []
        self.discover()
        for package in self.packages.values():
            package.transformed = self.transform(package)

    def add(self, manifest: Path, group: str, verifier: GitInput | None = None, revision: str | None = None) -> None:
        manifest = manifest.resolve()
        document = read_toml(manifest)
        original = document["package"]["name"]
        if original in self.excluded:
            return
        require(original not in self.packages, f"duplicate source package: {original}")
        workspace_root, workspace = workspace_for(manifest, document)
        version = document["package"]["version"]
        if isinstance(version, dict):
            version = workspace.get("package", {}).get("version")
        require(isinstance(version, str), f"source package has no explicit inherited version: {original}")
        default = original if group == "first-party" else "lintel-" + original.replace("_", "-")
        name = safe_package_name(self.settings.get("package_names", {}).get(original, default))
        require(all(p.name != name for p in self.packages.values()), f"duplicate registry target package: {name}")
        package = Package(original, name, version, manifest, workspace_root, workspace, document, group, verifier, revision)
        self.packages[original] = package
        self.by_manifest[manifest] = package

    def source(self, key: str) -> tuple[Path, GitInput | None, str | None]:
        source = self.settings.get("sources", {}).get(key)
        require(isinstance(source, dict) and source.get("path"), f"source-map is missing sources.{key}.path")
        path = Path(source["path"]).expanduser().resolve()
        require(path.is_dir(), f"configured source directory is missing: {key}")
        revision = source.get("revision")
        if key in PINS:
            require(revision == PINS[key], f"source-map revision must match the audited pin: {key}")
            verifier = GitInput(path, revision)
        else:
            require(source.get("registry_version"), f"registry snapshot must declare its version: {key}")
            info = read_json(path / ".cargo_vcs_info.json")
            actual = info.get("git", {}).get("sha1")
            require(isinstance(actual, str) and bool(re.fullmatch("[0-9a-f]{40}", actual)), "registry snapshot has no VCS revision")
            require(revision is None or revision == actual, f"registry snapshot revision mismatch: {key}")
            revision, verifier = actual, None
            actual_version = read_toml(path / "Cargo.toml")["package"]["version"]
            require(actual_version == source["registry_version"], f"registry snapshot version mismatch: {key}")
        self.inputs.append({"source": key, "directory": str(path), "revision": revision})
        return path, verifier, revision

    def discover(self) -> None:
        manifests = [self.root / "Cargo.toml", *sorted((self.root / "crates").glob("*/Cargo.toml")),
                     self.root / "crates/lintel-desktop/native-platform/Cargo.toml"]
        for manifest in manifests:
            self.add(manifest, "first-party")
        iced = self.root / "vendor/iced"
        marker = read_json(iced / ".lintel-iced-profile.json")
        require(marker.get("revision") == ICED_REVISION, "vendored Iced revision mismatch")
        patch = self.root / "profiles/macos-public/iced.patch"
        require(marker.get("patch_sha256") == sha256(patch.read_bytes()), "vendored Iced patch marker mismatch")
        for path, digest in marker.get("files", {}).items():
            require(sha256((iced / safe_relative(path)).read_bytes()) == digest, "vendored Iced source marker mismatch")
        self.inputs.append({"source": "iced", "directory": str(iced), "revision": ICED_REVISION,
                            "patch_sha256": marker["patch_sha256"]})
        iced_workspace = read_toml(iced / "Cargo.toml")["workspace"]
        for name, dependency in iced_workspace["dependencies"].items():
            if name.startswith("iced") and isinstance(dependency, dict) and "path" in dependency:
                self.add(iced / dependency["path"] / "Cargo.toml", "iced", revision=ICED_REVISION)
        for source_key, selected in [("wgpu", set(WGPU_PACKAGES)), ("winit", {"winit"}),
                                     ("cef", {"cef"}), ("cryoglyph", {"cryoglyph"}),
                                     ("accesskit_winit", {"accesskit_winit"})]:
            path, verifier, revision = self.source(source_key)
            found = set()
            for manifest in sorted(path.rglob("Cargo.toml")):
                if any(part in SKIP_DIRECTORIES for part in manifest.relative_to(path).parts):
                    continue
                document = read_toml(manifest)
                name = document.get("package", {}).get("name")
                if name in selected:
                    self.add(manifest, source_key, verifier, revision)
                    found.add(name)
            require(found == selected, f"source tree is missing expected packages: {source_key}: {sorted(selected - found)}")

    def dependency(self, owner: Package, key: str, spec: Any, kind: str) -> Any:
        local = {"version": spec} if isinstance(spec, str) else copy.deepcopy(spec)
        require(isinstance(local, dict), f"invalid dependency table: {owner.original}/{key}")
        base = owner.manifest.parent
        if local.pop("workspace", False):
            inherited = owner.workspace.get("dependencies", {}).get(key)
            require(inherited is not None, f"missing inherited dependency: {owner.original}/{key}")
            inherited = {"version": inherited} if isinstance(inherited, str) else copy.deepcopy(inherited)
            features = [*inherited.get("features", []), *local.get("features", [])]
            local = {**inherited, **local}
            if features:
                local["features"] = list(dict.fromkeys(features))
            base = owner.workspace_root
        original = local.get("package", key)
        omission = self.settings.get("omitted_dev_dependencies", {}).get(owner.original, [])
        if kind == "dev-dependencies" and (key in omission or original in omission):
            owner.omissions.append(key)
            return None
        if "path" in local:
            target = (base / local["path"] / "Cargo.toml").resolve()
            if target in self.by_manifest:
                original = self.by_manifest[target].original
            else:
                require(target.is_file(), f"dependency path is missing: {owner.original}/{key}")
                original = read_toml(target)["package"]["name"]
                require(original in self.overrides, f"unknown path dependency ({kind}): {owner.original}/{key} -> {original}")
        replacement = self.packages.get(original)
        override = self.overrides.get(original)
        require(not (replacement and override), f"package cannot be both staged and a registry override: {original}")
        if replacement:
            local["path"] = "../" + replacement.name
            local["version"] = "=" + replacement.version
            # cargo package resolves registry identities for dev dependencies too,
            # even when the package verification build does not compile its tests.
            owner.dependencies.add(replacement.name)
            name = replacement.name
        elif override:
            local.pop("path", None)
            local["version"] = override["version"]
            name = override.get("package", original)
        else:
            require("git" not in local, f"unknown Git dependency ({kind}): {owner.original}/{key} -> {original}")
            require("path" not in local, f"unknown path dependency ({kind}): {owner.original}/{key} -> {original}")
            require("version" in local, f"dependency lacks a registry version: {owner.original}/{key}")
            name = original
        require(local.get("registry", "crates-io") == "crates-io", "custom registries require a separate explicit release policy")
        for field in ["git", "rev", "branch", "tag", "registry"]:
            local.pop(field, None)
        if name != key:
            local["package"] = name
        else:
            local.pop("package", None)
        return local

    def transform(self, owner: Package) -> dict[str, Any]:
        result = copy.deepcopy(owner.document)
        package = result["package"]
        for field, value in list(package.items()):
            if isinstance(value, dict) and value.get("workspace") is True:
                require(field in owner.workspace.get("package", {}), f"missing inherited package field: {owner.original}/{field}")
                package[field] = copy.deepcopy(owner.workspace["package"][field])
        package.pop("workspace", None)
        package["name"], package["version"], package["publish"] = owner.name, owner.version, ["crates-io"]
        if "license" not in package and "license-file" not in package:
            license_id = self.settings.get("first_party_license") if owner.group == "first-party" else None
            require(isinstance(license_id, str) and license_id, f"source package has no declared license: {owner.original}")
            package["license"] = license_id
        if owner.group == "first-party":
            package.setdefault("repository", "https://github.com/looading/lintel")
        if (owner.manifest.parent / "src/lib.rs").is_file() or "lib" in result:
            result.setdefault("lib", {}).setdefault("name", owner.original.replace("-", "_"))
        lints = result.get("lints", {})
        if lints.pop("workspace", False):
            result["lints"] = {**copy.deepcopy(owner.workspace.get("lints", {})), **lints}
        result.pop("workspace", None)
        result.pop("patch", None)
        result.pop("replace", None)
        tables = [result]
        tables.extend(result.get("target", {}).values())
        for table in tables:
            for section in DEPENDENCY_SECTIONS:
                if section not in table:
                    continue
                converted = {}
                for key, dependency in table[section].items():
                    dependency = self.dependency(owner, key, dependency, section)
                    if dependency is not None:
                        converted[key] = dependency
                table[section] = converted
        overlay = self.resources.get(owner.original, {}).get("manifest", {})
        require(set(overlay) <= {"package"}, "resource manifest overrides may only change package file metadata")
        require(set(overlay.get("package", {})) <= {"include", "exclude", "readme", "license-file"},
                "resource metadata cannot change dependency or source behavior")
        deep_merge(result, overlay)
        render_toml(result)
        return result

    def plan(self) -> dict[str, Any]:
        pending = {package.name: set(package.dependencies) for package in self.packages.values()}
        order, layers, complete = [], [], set()
        while pending:
            ready = sorted(name for name, dependencies in pending.items() if dependencies <= complete)
            require(ready, f"cyclic registry publication dependencies: {sorted(pending)}")
            layers.append(ready)
            for name in ready:
                order.append(name)
                complete.add(name)
                del pending[name]
        packages = []
        for package in sorted(self.packages.values(), key=lambda package: order.index(package.name)):
            packages.append({"original_package": package.original, "package": package.name, "version": package.version,
                             "group": package.group, "source_manifest": str(package.manifest),
                             "source_revision": package.revision, "dependencies": sorted(package.dependencies),
                             "omitted_dev_dependencies": sorted(set(package.omissions)),
                             "manifest": f"packages/{package.name}/Cargo.toml"})
        return {"schema_version": SCHEMA_VERSION, "kind": "lintel.registry-release-plan", "package_count": len(packages),
                "publication_order": order, "layers": layers, "packages": packages,
                "registry_overrides": self.overrides, "inputs": self.inputs,
                "license_resources": [
                    {"package": package, "destination": item["destination"], "sha256": item["sha256"], "origin": item["origin"]}
                    for package, resources in self.resources.items() for item in resources.get("licenses", [])
                ],
                "executed_tools": [], "published": False}

    def stage(self, destination: Path) -> dict[str, Any]:
        destination = destination.expanduser().resolve()
        require(not destination.is_relative_to(self.root), "registry stage must be outside the source repository")
        require(not destination.exists(), "stage destination must not exist; no existing output is replaced")
        require(destination.parent.is_dir(), "stage parent directory must already exist")
        plan = self.plan()
        with tempfile.TemporaryDirectory(prefix=".lintel-registry-stage-", dir=destination.parent) as temporary:
            temporary_root = Path(temporary)
            tree = temporary_root / "release"
            tree.mkdir()
            for package in self.packages.values():
                output = tree / "packages" / package.name
                output.mkdir(parents=True)
                copy_package_tree(package, package.manifest.parent, output)
                document = copy.deepcopy(package.transformed)
                self.complete_package_files(package, output, document)
                (output / "Cargo.toml").write_text(render_toml(document))
            workspace = {"workspace": {"resolver": "2", "members": ["packages/" + name for name in plan["publication_order"]]}}
            profiles = read_toml(self.root / "Cargo.toml").get("profile")
            if profiles:
                workspace["profile"] = copy.deepcopy(profiles)
            (tree / "Cargo.toml").write_text(render_toml(workspace))
            # Config supplied with --config is validated against explicit final stage paths.
            # These local verification paths are outside every published package payload.
            patches = {package.name: {"path": str(destination / "packages" / package.name)} for package in self.packages.values()}
            (tree / "registry-patches.toml").write_text(render_toml({"patch": {"crates-io": patches}}))
            (tree / "plan.json").write_text(json.dumps(plan, ensure_ascii=False, indent=2) + "\n")
            files = {}
            for path in sorted(tree.rglob("*")):
                if path.is_file():
                    files[path.relative_to(tree).as_posix()] = {"sha256": sha256(path.read_bytes()), "bytes": path.stat().st_size}
            (tree / "checksums.json").write_text(json.dumps({"schema_version": 1, "files": files}, indent=2) + "\n")
            tree.rename(destination)
        return {"stage": str(destination), "package_count": plan["package_count"], "checksums": str(destination / "checksums.json"), "published": False}

    def complete_package_files(self, package: Package, output: Path, document: dict[str, Any]) -> None:
        metadata = document["package"]
        original_metadata = package.document["package"]
        for key in ["readme", "license-file"]:
            value = metadata.get(key)
            if not isinstance(value, str):
                continue
            inherited = isinstance(original_metadata.get(key), dict)
            source = (package.workspace_root if inherited else package.manifest.parent) / value
            require(source.is_file(), f"package metadata file is missing: {package.original}/{key}")
            target = Path(value).name
            copy_file(package, source, output / target)
            metadata[key] = target
        for license_name in ["LICENSE", "LICENSE.MIT", "LICENSE.APACHE", "LICENSE-APACHE", "LICENSE-MIT", "COPYING", "NOTICE"]:
            if (output / license_name).is_file():
                continue
            source = package.workspace_root / license_name
            if source.is_file():
                copy_file(package, source, output / license_name)
        resource = self.resources.get(package.original, {})
        for item in resource.get("licenses", []):
            relative = safe_relative(item["destination"])
            require(len(relative.parts) == 1 and relative.name.upper().startswith(("LICENSE", "COPYING", "NOTICE")),
                    "license resources must use a top-level legal notice filename")
            require(bool(re.fullmatch("[0-9a-f]{64}", item.get("sha256", ""))), "license resources need an exact SHA-256")
            require(isinstance(item.get("origin"), str) and item["origin"].startswith("https://"),
                    "license resources need their verified public origin")
            source = Path(item["source"]).expanduser().resolve()
            require(source.is_file() and not source.is_symlink(), "license resource must be a real file")
            data = source.read_bytes()
            require(sha256(data) == item["sha256"], "license resource does not match its verified SHA-256")
            target = output / relative
            if target.exists():
                require(target.read_bytes() == data, "license resource would replace a different existing notice")
            else:
                target.write_bytes(data)
            if "include" in metadata:
                metadata["include"] = list(dict.fromkeys([*metadata["include"], relative.as_posix()]))
        for overlay in resource.get("overlays", []):
            source = Path(overlay["source"]).expanduser().resolve()
            relative = safe_relative(overlay["destination"])
            require(relative.parts[0] == "resources", "prepared overlays must stay within package resources")
            require(relative.suffix not in {".rs", ".wgsl", ".h", ".c", ".cpp"}, "prepared overlays cannot replace implementation source")
            target = output / relative
            require(source.exists(), f"prepared resource is missing for package: {package.original}")
            if source.is_dir():
                copy_overlay_tree(source, target)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(source.read_bytes())
            if "include" in metadata:
                metadata["include"] = list(dict.fromkeys([*metadata["include"], overlay["destination"]]))
        legal_files = [path for path in output.iterdir()
                       if path.is_file() and path.name.upper().startswith(("LICENSE", "COPYING"))]
        declared_file = metadata.get("license-file")
        require(legal_files or (isinstance(declared_file, str) and (output / declared_file).is_file()),
                f"package has no license text; supply a checksummed license resource: {package.original}")


def deep_merge(target: dict[str, Any], overlay: dict[str, Any]) -> None:
    for key, value in overlay.items():
        if isinstance(value, dict) and isinstance(target.get(key), dict):
            deep_merge(target[key], value)
        else:
            target[key] = copy.deepcopy(value)


def credential_name(path: Path) -> bool:
    name = path.name.lower()
    return (name in {".env", ".netrc", ".npmrc", ".git-credentials", "credentials.json", "id_rsa", "id_ed25519"}
            or name.startswith(".env.") or path.suffix.lower() in {".p12", ".pfx", ".pdb", ".dwarf", ".dwo", ".dwp"}
            or (path.parent.name.lower() == ".cargo" and name in {"credentials", "credentials.toml"}))


def copy_file(package: Package, source: Path, destination: Path) -> None:
    require(not source.is_symlink() and source.is_file(), "release sources must be real files")
    require(not credential_name(source), f"credential or symbol artifact in source package: {package.original}/{source.name}")
    data = source.read_bytes()
    if package.verifier:
        package.verifier.verify(source, data)
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(data)
    destination.chmod(source.stat().st_mode & 0o777)


def copy_package_tree(package: Package, source: Path, destination: Path) -> None:
    for entry in sorted(source.iterdir()):
        package_manifest = entry == package.manifest
        if entry.name in SKIP_DIRECTORIES or entry.name in SKIP_FILES or package_manifest:
            continue
        require(not entry.is_symlink(), f"symlink in source package: {package.original}/{entry.name}")
        if entry.is_dir():
            in_resources = entry.relative_to(package.manifest.parent).parts[0] == "resources"
            if not in_resources and (entry / "Cargo.toml").is_file():
                continue
            if entry.name.lower().endswith(".dsym"):
                raise ReleaseError(f"debug-symbol artifact in source package: {package.original}")
            child = destination / entry.name
            child.mkdir(exist_ok=True)
            copy_package_tree(package, entry, child)
        else:
            copy_file(package, entry, destination / entry.name)


def copy_overlay_tree(source: Path, destination: Path) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    for entry in sorted(source.iterdir()):
        require(not entry.is_symlink(), "prepared resources cannot contain symlinks")
        require(entry.name.lower() != ".git" and not credential_name(entry), "private artifact in prepared resources")
        target = destination / entry.name
        if entry.is_dir():
            copy_overlay_tree(entry, target)
        else:
            require(entry.suffix not in {".rs", ".wgsl", ".h", ".c", ".cpp"}, "prepared overlays cannot replace implementation source")
            target.write_bytes(entry.read_bytes())
            target.chmod(entry.stat().st_mode & 0o777)


def self_test() -> None:
    sample = {
        "package": {"name": "demo", "version": "0.1.0", "metadata": {"release": [{"role": "one", "args": ["a", "b"]}]}},
        "target": {'cfg(target_os = "macos")': {"dependencies": {"dep": {"version": "1", "default-features": False}}}},
        "bin": [{"name": "demo", "path": "src/main.rs"}, {"name": "worker", "path": "src/worker.rs"}],
        "lints": {"rust": {"unsafe_code": {"level": "warn", "priority": -1}}},
        "features": {"default": [], "native": ["dep:dep"]},
    }
    require(tomllib.loads(render_toml(sample)) == sample, "self-test: TOML roundtrip failed")
    for path in ["../escape", "/absolute", "a\\b", ".git/config"]:
        try:
            safe_relative(path)
        except ReleaseError:
            pass
        else:
            raise ReleaseError("self-test: unsafe resource path accepted")
    with tempfile.TemporaryDirectory(prefix="lintel-release-self-test-") as temporary:
        root = Path(temporary)
        manifest = root / "Cargo.toml"
        manifest.write_text('[package]\nname="original"\nversion="1.2.3"\n')
        (root / "src").mkdir()
        (root / "src/lib.rs").write_text("pub fn original() {}\n")
        document = {
            "package": {"name": "original", "version": {"workspace": True}, "license": {"workspace": True}},
            "dependencies": {"alias": {"workspace": True, "features": ["child"]}},
            "lints": {"workspace": True},
        }
        shared = {"package": {"version": "1.2.3", "license": "MIT"},
                  "dependencies": {"alias": {"package": "target", "version": "1", "features": ["inherited"]}},
                  "lints": {"rust": {"unsafe_code": "warn"}}}
        owner = Package("original", "lintel-original", "1.2.3", manifest, root, shared, document, "fork")
        target = Package("target", "lintel-target", "1.2.3", root / "target/Cargo.toml", root, {}, {}, "fork")
        stager = object.__new__(Stager)
        stager.root, stager.settings, stager.resources = root, {}, {}
        stager.packages, stager.by_manifest, stager.overrides = {"target": target}, {}, {}
        result = stager.transform(owner)
        require(result["package"]["version"] == "1.2.3" and result["lib"]["name"] == "original", "self-test: library identity changed")
        require(result["dependencies"]["alias"] == {
            "package": "lintel-target", "version": "=1.2.3", "path": "../lintel-target",
            "features": ["inherited", "child"]}, "self-test: dependency alias or additive features changed")
        require(result["lints"] == shared["lints"], "self-test: inherited lints changed")
        for dependency in [{"git": "https://example.invalid/unmapped"}, {"path": "missing"}]:
            try:
                stager.dependency(owner, "unmapped", dependency, "dependencies")
            except ReleaseError:
                pass
            else:
                raise ReleaseError("self-test: unknown dependency accepted")
        stager.resources = {"original": {"manifest": {"dependencies": {"injected": "1"}}}}
        try:
            stager.transform(owner)
        except ReleaseError:
            pass
        else:
            raise ReleaseError("self-test: resource metadata changed dependencies")
        stager.resources = {}
        output = root / "output"
        output.mkdir()
        try:
            stager.complete_package_files(owner, output, copy.deepcopy(result))
        except ReleaseError as error:
            require("no license text" in str(error), "self-test: missing license did not fail specifically")
        else:
            raise ReleaseError("self-test: missing license accepted")
        notice = root / "verified-notice.txt"
        notice.write_bytes(b"test-only legal notice\n")
        stager.resources = {"original": {"licenses": [{"source": str(notice), "destination": "LICENSE-APACHE",
            "sha256": sha256(notice.read_bytes()), "origin": "https://example.invalid/verified-license"}]}}
        stager.complete_package_files(owner, output, copy.deepcopy(result))
        require((output / "LICENSE-APACHE").read_bytes() == notice.read_bytes(), "self-test: license bytes changed")
        stager.resources["original"]["licenses"][0]["sha256"] = "0" * 64
        try:
            stager.complete_package_files(owner, output, copy.deepcopy(result))
        except ReleaseError as error:
            require("SHA-256" in str(error), "self-test: license checksum did not fail specifically")
        else:
            raise ReleaseError("self-test: unverified license accepted")
        stager.resources, stager.inputs = {}, []
        stager.packages = {owner.original: owner, target.original: target}
        owner.dependencies.clear()
        stager.dependency(owner, "dev_alias", {"package": "target", "version": "1"}, "dev-dependencies")
        plan = stager.plan()
        require(plan["publication_order"].index(target.name) < plan["publication_order"].index(owner.name),
                "self-test: internal dev prerequisite was published too late")
        target.dependencies.add(owner.name)
        try:
            stager.plan()
        except ReleaseError as error:
            require("cyclic" in str(error), "self-test: dependency cycle did not fail specifically")
        else:
            raise ReleaseError("self-test: publication dependency cycle accepted")
        target.dependencies.clear()
        stager.settings["omitted_dev_dependencies"] = {owner.original: [owner.original]}
        omitted = stager.dependency(owner, "self_test", {"package": owner.original, "version": "1"}, "dev-dependencies")
        require(omitted is None and owner.name not in owner.dependencies,
                "self-test: explicit self-referential dev omission was not honored")
        require(len(stager.plan()["publication_order"]) == 2, "self-test: explicit omission introduced a cycle")
    print(json.dumps({"self_test": "passed", "checks": ["TOML nested metadata", "array tables", "target cfg", "lints", "resource paths", "workspace flattening", "library and dependency aliases", "additive features", "unknown dependency rejection", "resource isolation", "license presence and provenance", "internal dev publication prerequisites", "cycle rejection and explicit self-dev omission"], "executed_tools": []}))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--source-map", type=Path)
    parser.add_argument("--resources-manifest", type=Path)
    parser.add_argument("--exclude", action="append", default=[], metavar="ORIGINAL_PACKAGE")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--plan", action="store_true", help="print the resolved publication plan without writing files")
    mode.add_argument("--stage", type=Path, metavar="OUT", help="write a new external release workspace")
    mode.add_argument("--self-test", action="store_true", help="check conversion primitives without source discovery")
    args = parser.parse_args()
    try:
        if args.self_test:
            self_test()
            return 0
        require(args.source_map is not None, "--source-map is required for explicit registry equivalence decisions")
        stager = Stager(args.root.expanduser().resolve(), read_json(args.source_map),
                        read_json(args.resources_manifest) if args.resources_manifest else {}, set(args.exclude))
        result = stager.stage(args.stage) if args.stage else stager.plan()
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return 0
    except (ReleaseError, OSError, ValueError, KeyError, tomllib.TOMLDecodeError) as error:
        print(json.dumps({"error": str(error), "published": False, "executed_tools": []}), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
