"""Build hook for the jqf wheel.

The package is pure Python but useless without the cdylib, so the wheel
build shells out to cargo and bundles `libjqf_sdk_ffi` inside the package.
The wheel is therefore PLATFORM-specific even though no
Python extension is compiled, which is what the `BdistWheel` tag override
says. Version alignment: the package version is derived from
`jqf-sdk-ffi/Cargo.toml` HERE and nowhere else (following
`version.workspace = true` to the root manifest's `[workspace.package]`) —
cargo semver `0.1.0-alpha.1` maps to PEP 440 `0.1.0a1`.
"""
import re
import subprocess
from pathlib import Path

import setuptools
from setuptools import setup
from setuptools.command.build_py import build_py
from wheel.bdist_wheel import bdist_wheel

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent.parent
LIB_NAMES = ["libjqf_sdk_ffi.dylib", "libjqf_sdk_ffi.so", "jqf_sdk_ffi.dll"]


def pep440(cargo_version):
    # PEP 440 has no "-alpha.N" spelling: 0.1.0-alpha.1 -> 0.1.0a1
    return re.sub(r"[.-](alpha|beta|rc)\.(\d+)",
                  lambda m: m.group(1)[0] + m.group(2), cargo_version)


def cargo_version():
    text = (REPO / "jqf-sdk-ffi" / "Cargo.toml").read_text()
    m = re.search(r'^version = "([^"]+)"', text, re.M)
    if m:
        return m.group(1)
    # A workspace-inherited version (`version.workspace = true`) lives under [workspace.package] in the ROOT manifest,
    # nowhere in the crate's own.
    assert re.search(r"^version\.workspace\s*=\s*true", text, re.M), (
        "no version and no workspace inheritance in jqf-sdk-ffi/Cargo.toml")
    root = (REPO / "Cargo.toml").read_text()
    section = re.search(r"^\[workspace\.package\]([^\[]*)", root, re.S | re.M)
    assert section, "no [workspace.package] in the root Cargo.toml"
    m = re.search(r'^version = "([^"]+)"', section.group(1), re.M)
    assert m, "no version under [workspace.package]"
    return m.group(1)


def version():
    return pep440(cargo_version())


def cdylib():
    target = REPO / "target" / "release"
    for name in LIB_NAMES:
        if (target / name).exists():
            return target / name
    subprocess.run(["cargo", "build", "--release", "-p", "jqf-sdk-ffi"],
                   cwd=REPO, check=True)
    for name in LIB_NAMES:
        if (target / name).exists():
            return target / name
    raise SystemExit("cargo did not produce a cdylib under target/release")


class BuildPy(build_py):
    def run(self):
        super().run()
        src = cdylib()
        dest = Path(self.build_lib) / "jqf" / src.name
        self.mkpath(str(dest.parent))
        self.copy_file(str(src), str(dest))


class BdistWheel(bdist_wheel):
    def finalize_options(self):
        super().finalize_options()
        self.root_is_pure = False

    def get_tag(self):
        # Pure Python over a plain C cdylib: no Python ABI, so the wheel must not be pinned to one interpreter (cp314);
        # the platform tag (macosx_*/manylinux_*) is what makes it installable only where the cdylib runs.
        python, abi, plat = super().get_tag()
        return "py3", "none", plat


def _pep621_capable():
    try:
        major, minor = (int(part) for part in setuptools.__version__.split(".")[:2])
    except (ValueError, AttributeError):
        return True
    return (major, minor) >= (61, 0)


# PEP 621 ([project] in pyproject.toml) needs setuptools >= 61. Older setuptools (the macOS system Python ships 58)
# silently IGNORES that table AND [tool.setuptools], so a bare setup() builds an UNKNOWN-named wheel with no package
# inside; those hosts get the name and the package list passed explicitly instead. Newer setuptools keeps the table-only
# path and must not see conflicting keywords.
_SETUP_KWARGS = {} if _pep621_capable() else {
    "name": "jqf",
    "packages": ["jqf"],
}

setup(version=version(),
      cmdclass={"build_py": BuildPy, "bdist_wheel": BdistWheel},
      **_SETUP_KWARGS)
