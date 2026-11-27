#!/usr/bin/env python3
"""Checks for Flatpak source, application metadata, and publication input."""

import configparser
import json
import pathlib
import re
import sys
import tarfile
import xml.etree.ElementTree as ElementTree

APP_ID = "computer.footy.app"

# The complete sandbox permission list. Changing it is a reviewed decision.
FINISH_ARGS = {
    "--share=ipc",
    "--share=network",
    "--socket=wayland",
    "--socket=fallback-x11",
    "--socket=pulseaudio",
    "--device=dri",
    "--talk-name=org.kde.StatusNotifierWatcher",
}

# The same permissions as they appear in the built application's metadata.
CONTEXT = {
    "shared": {"network", "ipc"},
    "sockets": {"wayland", "fallback-x11", "pulseaudio"},
    "devices": {"dri"},
}
SESSION_BUS = {"org.kde.StatusNotifierWatcher": "talk"}


def fail(message):
    sys.exit(f"check failed: {message}")


def unpack_archive(path, destination, max_bytes):
    """Extract one archive-z2 repository into an empty, private directory."""
    names = set()
    members = []
    size = 0
    with tarfile.open(path, "r:gz") as archive:
        for member in archive:
            parts = member.name.split("/")
            if parts[0] != "repo" or any(part in ("", ".", "..") for part in parts):
                fail(f"invalid archive member path: {member.name!r}")
            if member.name in names:
                fail(f"duplicate archive member: {member.name!r}")
            names.add(member.name)
            if member.type not in (tarfile.REGTYPE, tarfile.AREGTYPE, tarfile.DIRTYPE):
                fail(f"unsupported archive member type: {member.name!r}")
            size += member.size
            if member.size < 0 or size > max_bytes:
                fail("archive exceeds the extracted size limit")
            if member.name == "repo/config":
                if not member.isfile():
                    fail("archive repo config is not a regular file")
                continue
            members.append(member)
        # Use the same parser for validation and extraction, including PAX paths.
        archive.extractall(destination, members=members, filter="data")


def modules(entries):
    for module in entries:
        if not isinstance(module, dict):
            fail(f"unresolved module include: {module!r}")
        yield module
        yield from modules(module.get("modules", []))


def check_manifest(stream):
    """Check `flatpak-builder --show-manifest` output."""
    manifest = json.load(stream)
    if manifest.get("app-id", manifest.get("id")) != APP_ID:
        fail("unexpected application ID")
    finish_args = manifest.get("finish-args", [])
    if sorted(finish_args) != sorted(FINISH_ARGS):
        fail(f"finish-args differ from the allowlist: {sorted(finish_args)}")
    for module in modules(manifest["modules"]):
        name = module.get("name")
        for source in module.get("sources", []):
            if not isinstance(source, dict):
                fail(f"{name}: unresolved source include: {source!r}")
            kind = source.get("type")
            if kind in ("archive", "file"):
                if "url" in source and not re.fullmatch(
                    r"[0-9a-f]{64}", source.get("sha256", "")
                ):
                    fail(f"{name}: download without a SHA-256: {source.get('url')}")
            elif kind == "git":
                if not re.fullmatch(r"[0-9a-f]{40}", source.get("commit", "")):
                    fail(f"{name}: git source without a full commit hash")
            elif kind not in ("dir", "patch"):
                fail(f"{name}: unexpected source type {kind!r}")


def check_metadata(path):
    """Check a built application's metadata file."""
    parser = configparser.ConfigParser(
        delimiters=("=",), interpolation=None, strict=True
    )
    parser.optionxform = str
    parser.read_string(pathlib.Path(path).read_text(encoding="utf-8"))

    allowed = {
        "Application",
        "Context",
        "Session Bus Policy",
        f"Extension {APP_ID}.Debug",
        "Build",
    }
    extra = set(parser.sections()) - allowed
    if extra:
        fail(f"unexpected metadata sections: {sorted(extra)}")
    for section in ("Application", "Context", "Session Bus Policy"):
        if section not in parser:
            fail(f"missing metadata section: {section}")

    application = parser["Application"]
    expected = {
        "name": APP_ID,
        "runtime": "org.gnome.Platform/x86_64/51",
        "command": "footy",
    }
    for key, value in expected.items():
        if application.get(key) != value:
            fail(f"[Application] {key} is {application.get(key)!r}, expected {value!r}")

    context = {
        key: {item for item in value.split(";") if item}
        for key, value in parser["Context"].items()
    }
    # Some Flatpak versions also record "x11" next to "fallback-x11".
    if "fallback-x11" in context.get("sockets", set()):
        context["sockets"].discard("x11")
    if context != CONTEXT:
        fail(f"sandbox permissions differ from the allowlist: {context}")
    session_bus = dict(parser["Session Bus Policy"])
    if session_bus != SESSION_BUS:
        fail(f"session bus policy differs from the allowlist: {session_bus}")


def check_version(source_dir):
    """The newest AppStream release must match the application version."""
    source = pathlib.Path(source_dir)
    config = json.loads((source / "src-tauri/tauri.conf.json").read_text())
    metainfo = ElementTree.parse(source / f"packaging/flatpak/{APP_ID}.metainfo.xml")
    release = metainfo.find("./releases/release")
    if release is None or release.get("version") != config["version"]:
        found = None if release is None else release.get("version")
        fail(
            f"metainfo release {found} does not match tauri.conf.json "
            f"version {config['version']}"
        )
    if config["identifier"] != APP_ID:
        fail("tauri.conf.json identifier does not match the Flatpak application ID")


if __name__ == "__main__":
    match sys.argv[1:]:
        case ["manifest"]:
            check_manifest(sys.stdin)
        case ["metadata", path]:
            check_metadata(path)
        case ["version", source_dir]:
            check_version(source_dir)
        case ["unpack", archive, destination, max_bytes]:
            unpack_archive(archive, destination, int(max_bytes))
        case _:
            sys.exit(
                "usage: check.py manifest | metadata FILE | version SOURCE_DIR "
                "| unpack ARCHIVE DESTINATION MAX_BYTES"
            )
