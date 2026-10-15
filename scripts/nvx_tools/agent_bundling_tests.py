"""Hypervisor-free check of edge agent bundle staging in the aci_edge_sandboxes build.

The check writes a synthetic bundle whose files hold their own names, requires the
bundled-agent unit tests to find it with the digests that the build compiled in, and
requires the build to reject broken copies of it. Neither needs an agent library that
runs or a hypervisor.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from .build_constants import BuildConstants
from .common import ScriptError, sha256_file

CRATE_DIRECTORY = BuildConstants.REPO_ROOT / "aci_edge_sandboxes"
# The names that the crate's build script and library share.
CONTRACT_SOURCE = CRATE_DIRECTORY / "src" / "agent" / "bundle_contract.rs"
# The sandbox ABI that the crate speaks.
WIRE_SOURCE = CRATE_DIRECTORY / "model" / "src" / "wire.rs"
CHECKSUMS = "SHA256SUMS"
BUNDLE_MANIFEST = "bundle-manifest.json"
KERNEL = "guest/vmlinux"
INITRAMFS = "guest/initramfs-edge.cpio.gz"
# The contract's variables that would select another bundle, or none, than the check's.
OTHER_SOURCES = (
    "BUNDLE_ENV",
    "BUNDLE_URL_ENV",
    "BUNDLE_SHA256_ENV",
    "ARTIFACTS_DIR_ENV",
)


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.description = (
        "Check that the aci_edge_sandboxes build stages a synthetic edge agent bundle, "
        "which the bundled-agent tests must find with the digests that were compiled "
        "in, and rejects a tampered file, a wrong pin, another sandbox ABI, and a link."
    )
    parser.add_argument(
        "--cargo",
        default="cargo",
        help="cargo executable used to build and test the crate (default: cargo)",
    )
    parser.add_argument(
        "--toolchain",
        help="Rust toolchain to run cargo with, such as 1.95.0 (default: rustup's)",
    )
    parser.set_defaults(handler=command_test_agent_bundling)


def contract_names(source: Path = CONTRACT_SOURCE) -> dict[str, str]:
    """Returns the string constants of the crate's bundle contract by name."""
    pattern = re.compile(
        r'^pub\(crate\) const ([A-Z0-9_]+): &str = "([^"\\]*)";$', re.M
    )
    names = dict(pattern.findall(source.read_text(encoding="utf-8")))
    if not names:
        raise ScriptError(f"{source} declares no names")
    return names


def sandbox_abi_version(source: Path = WIRE_SOURCE) -> int:
    """Returns the sandbox ABI version that the crate's data model declares."""
    match = re.search(
        r"^pub const SANDBOX_ABI_VERSION: u32 = (\d+);$",
        source.read_text(encoding="utf-8"),
        re.M,
    )
    if match is None:
        raise ScriptError(f"{source} declares no SANDBOX_ABI_VERSION")
    return int(match.group(1))


@dataclass(frozen=True)
class BundleLayout:
    """The names that a bundle for one host platform gives its executables."""

    executable_suffix: str
    library: str

    @classmethod
    def for_platform(cls, platform: str = sys.platform) -> BundleLayout:
        if platform == "win32":
            return cls(".exe", "bin/aci_edge_agent.dll")
        return cls("", "bin/libaci_edge_agent.so")

    @property
    def openvmm(self) -> str:
        return f"bin/openvmm{self.executable_suffix}"

    @property
    def direct_images(self) -> str:
        return f"bin/direct-images{self.executable_suffix}"

    def files(self) -> tuple[str, ...]:
        return (self.openvmm, self.direct_images, self.library, KERNEL, INITRAMFS)


def write_bundle(
    directory: Path,
    layout: BundleLayout,
    source_manifest: str,
    sandbox_abi: int,
    *,
    kernel: str = KERNEL,
) -> None:
    """Writes a bundle into `directory` whose files hold their own names.

    `source_manifest` names the runtime manifest, `kernel` names the file whose digest
    that manifest pins as the kernel's, and `sandbox_abi` is the sandbox ABI that the
    bundle declares for its library.
    """
    for name in layout.files():
        path = directory / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(name.encode())

    def digest(name: str) -> str:
        return sha256_file(directory / name)

    def write_json(name: str, value: object) -> None:
        (directory / name).write_bytes(json.dumps(value, indent=2).encode() + b"\n")

    write_json(
        source_manifest,
        {
            "format": 2,
            "profile": "edge",
            "openvmm": {"executable_sha256": digest(layout.openvmm)},
            "linux": {"kernel_sha256": digest(kernel)},
            "edge": {"initramfs_sha256": digest(INITRAMFS)},
            "direct_images": {"executable_sha256": digest(layout.direct_images)},
            "agent": {"library_sha256": digest(layout.library)},
        },
    )
    write_json(
        BUNDLE_MANIFEST,
        {
            "status": {"complete": True},
            "consumer": {
                "runtimeBundleManifest": source_manifest,
                "runtimeBundleManifestSha256": digest(source_manifest),
                "backendLibrary": layout.library,
                "backendLibrarySha256": digest(layout.library),
                "sandboxAbiVersion": sandbox_abi,
            },
        },
    )
    listed = (*layout.files(), source_manifest, BUNDLE_MANIFEST)
    checksums = "".join(f"{digest(name)}  {name}\n" for name in listed)
    (directory / CHECKSUMS).write_bytes(checksums.encode())


def cargo_command(
    cargo: str, toolchain: str | None, subcommand: str, *arguments: str
) -> list[str]:
    """Returns a cargo command for the crate's library with `agent-bundled`."""
    return [
        cargo,
        *([f"+{toolchain}"] if toolchain else []),
        subcommand,
        "--manifest-path",
        os.fspath(CRATE_DIRECTORY / "Cargo.toml"),
        "--locked",
        "--lib",
        "--features",
        "agent-bundled",
        *arguments,
    ]


def bundle_environment(names: dict[str, str], bundle: Path) -> dict[str, str]:
    """Returns this process's environment with `bundle` as the only bundle source."""
    environment = os.environ.copy()
    for name in OTHER_SOURCES:
        environment.pop(names[name], None)
    environment[names["BUNDLE_DIR_ENV"]] = os.fspath(bundle)
    return environment


def link_directory(link: Path, target: Path) -> None:
    """Makes `link` a link to the directory `target`.

    Windows gets a junction, which needs no privilege; other hosts a symbolic link.
    """
    if sys.platform == "win32":
        import _winapi

        _winapi.CreateJunction(os.fspath(target), os.fspath(link))
    else:
        link.symlink_to(target, target_is_directory=True)


def expect_rejection(
    command: list[str], environment: dict[str, str], bundle: str, message: str
) -> None:
    """Fails unless `command` fails and reports `message` for the `bundle` bundle."""
    print(f">> {bundle} bundle: {shlex.join(command)}", flush=True)
    result = subprocess.run(
        command,
        env=environment,
        check=False,
        capture_output=True,
        encoding="utf-8",
        errors="replace",
    )
    if result.returncode == 0:
        raise ScriptError(f"the build accepted the {bundle} bundle")
    if message not in result.stdout + result.stderr:
        print(result.stdout + result.stderr, file=sys.stderr)
        raise ScriptError(
            f"the build rejected the {bundle} bundle without reporting: {message}"
        )


def command_test_agent_bundling(args: argparse.Namespace) -> int:
    cargo = cast(str, args.cargo)
    toolchain = cast("str | None", args.toolchain)
    names = contract_names()
    sandbox_abi = sandbox_abi_version()
    layout = BundleLayout.for_platform()
    manifest = names["SOURCE_MANIFEST"]
    check = cargo_command(cargo, toolchain, "check")
    with tempfile.TemporaryDirectory(prefix="aci-edge-agent-bundle-") as temporary:
        root = Path(temporary)
        good = root / "good"
        write_bundle(good, layout, manifest, sandbox_abi)
        test = cargo_command(cargo, toolchain, "test", "bundled")
        print(f">> {shlex.join(test)}", flush=True)
        environment = bundle_environment(names, good)
        status = subprocess.run(test, env=environment, check=False).returncode
        if status != 0:
            return status

        tampered = root / "tampered"
        shutil.copytree(good, tampered)
        with (tampered / KERNEL).open("ab") as kernel:
            kernel.write(b"x")
        environment = bundle_environment(names, tampered)
        expect_rejection(check, environment, "tampered", "SHA-256 mismatch")

        pinned = root / "pinned"
        write_bundle(pinned, layout, manifest, sandbox_abi, kernel=INITRAMFS)
        expect_rejection(
            check,
            bundle_environment(names, pinned),
            "wrongly pinned",
            "pins /linux/kernel_sha256 to another digest",
        )

        other_abi = root / "other-abi"
        write_bundle(other_abi, layout, manifest, sandbox_abi + 1)
        expect_rejection(
            check,
            bundle_environment(names, other_abi),
            "other-ABI",
            f"speaks sandbox ABI {sandbox_abi + 1}",
        )

        linked = root / "linked"
        shutil.copytree(good, linked)
        (linked / "guest").rename(root / "guest")
        link_directory(linked / "guest", root / "guest")
        expect_rejection(
            check, bundle_environment(names, linked), "linked", "is a link"
        )
    print("The build staged the agent bundle and rejected its broken copies.")
    return 0
