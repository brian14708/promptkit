from __future__ import annotations

import asyncio
import copy
import errno
import hashlib
import io
import os
import shutil
import tarfile
import tempfile
from importlib.metadata import version as _pkg_version
from pathlib import Path, PurePosixPath
from typing import Literal

import httpx2

from isola._core import TemplateConfig

RuntimeName = Literal["python", "js"]

_BUNDLE_FILES: dict[str, str] = {"python": "python.wasm", "js": "js.wasm"}
_TARBALL_NAMES: dict[str, str] = {
    "python": "isola-python-runtime.tar.gz",
    "js": "isola-js-runtime.tar.gz",
}
_RELEASE_API = "https://api.github.com/repos/brian14708/isola/releases/tags/{version}"


def _version_tag(ver: str) -> str:
    if ver.startswith("v"):
        return ver
    return f"v{ver}"


def _cache_base() -> Path:
    xdg = os.environ.get("XDG_CACHE_HOME")
    if xdg:
        return Path(xdg)
    return Path.home() / ".cache"


async def resolve_runtime(
    runtime: RuntimeName, *, version: str | None = None
) -> TemplateConfig:
    if runtime not in _BUNDLE_FILES:
        msg = f"unknown runtime: {runtime!r}"
        raise ValueError(msg)

    if version is None:
        version = _pkg_version("isola")

    cache_dir = _cache_base() / "isola" / "runtimes" / f"{runtime}-{version}"
    check_path = cache_dir / "bin" / _BUNDLE_FILES[runtime]

    if check_path.is_file():
        return _build_config(runtime, cache_dir)

    # Download and extract.
    tarball_name = _TARBALL_NAMES[runtime]
    expected_digest = await _fetch_expected_digest(version, tarball_name)
    tarball_bytes = await _download_tarball(version, tarball_name, expected_digest)
    await _extract_tarball(tarball_bytes, cache_dir)

    if not check_path.is_file():
        msg = f"downloaded runtime is missing {_BUNDLE_FILES[runtime]!r}"
        raise RuntimeError(msg)

    return _build_config(runtime, cache_dir)


def _build_config(runtime: RuntimeName, cache_dir: Path) -> TemplateConfig:
    if runtime == "python":
        return TemplateConfig(
            runtime_path=cache_dir / "bin", runtime_lib_dir=cache_dir / "lib"
        )
    return TemplateConfig(runtime_path=cache_dir / "bin")


async def _fetch_expected_digest(version: str, tarball_name: str) -> str:
    url = _RELEASE_API.format(version=_version_tag(version))
    async with httpx2.AsyncClient() as client:
        resp = await client.get(url)
        resp.raise_for_status()
        release = resp.json()

    for asset in release.get("assets", []):
        if asset.get("name") == tarball_name:
            digest: str | None = asset.get("digest")
            if digest is None:
                msg = f"no digest found for asset {tarball_name!r} in release {version}"
                raise RuntimeError(msg)
            return digest

    msg = f"asset {tarball_name!r} not found in release {version}"
    raise RuntimeError(msg)


async def _download_tarball(
    version: str, tarball_name: str, expected_digest: str
) -> bytes:
    download_url = (
        "https://github.com/brian14708/isola"
        f"/releases/download/{_version_tag(version)}/{tarball_name}"
    )
    sha = hashlib.sha256()
    chunks: list[bytes] = []

    async with (
        httpx2.AsyncClient(follow_redirects=True) as client,
        client.stream("GET", download_url) as resp,
    ):
        resp.raise_for_status()
        async for chunk in resp.aiter_bytes():
            sha.update(chunk)
            chunks.append(chunk)

    actual = f"sha256:{sha.hexdigest()}"
    if actual != expected_digest:
        msg = (
            f"digest mismatch for {tarball_name}: "
            f"expected {expected_digest}, got {actual}"
        )
        raise RuntimeError(msg)

    return b"".join(chunks)


async def _extract_tarball(data: bytes, dest: Path) -> None:
    def _do_extract() -> None:
        dest.parent.mkdir(parents=True, exist_ok=True)
        tmp_dir = tempfile.mkdtemp(dir=dest.parent, prefix=f".{dest.name}-")
        try:
            with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tf:
                symlink_paths: set[PurePosixPath] = set()
                for member in tf.getmembers():
                    stripped_name = _strip_first_path_component(member.name)
                    if stripped_name is None:
                        continue

                    _validate_tar_member(member, stripped_name, symlink_paths)

                    extracted_member = copy.copy(member)
                    extracted_member.name = stripped_name
                    tf.extract(extracted_member, tmp_dir)
            try:
                Path(tmp_dir).rename(dest)
            except OSError as exc:
                if exc.errno not in {errno.EEXIST, errno.ENOTEMPTY}:
                    raise
                shutil.rmtree(tmp_dir, ignore_errors=True)
        except BaseException:
            shutil.rmtree(tmp_dir, ignore_errors=True)
            raise

    await asyncio.to_thread(_do_extract)


def _validate_symlink_target(
    member_path: PurePosixPath, link_name: str, member_name: str
) -> None:
    normalized_link = link_name.replace("\\", "/")
    link_path = PurePosixPath(normalized_link)
    if link_path.is_absolute() or (
        len(normalized_link) >= 3
        and normalized_link[0].isalpha()
        and normalized_link[1:3] == ":/"
    ):
        msg = f"symlink target escapes archive: {member_name!r}"
        raise RuntimeError(msg)

    depth = len(member_path.parent.parts)
    for part in link_path.parts:
        if part in {"", "."}:
            continue
        if part == "..":
            depth -= 1
            if depth < 0:
                msg = f"symlink target escapes archive: {member_name!r}"
                raise RuntimeError(msg)
        else:
            depth += 1


def _validate_tar_member(
    member: tarfile.TarInfo, stripped_name: str, symlink_paths: set[PurePosixPath]
) -> None:
    member_path = PurePosixPath(stripped_name)
    if any(parent in symlink_paths for parent in member_path.parents):
        msg = f"archive entry traverses symlink: {member.name!r}"
        raise RuntimeError(msg)
    if member.islnk():
        msg = f"archive hard links are not supported: {member.name!r}"
        raise RuntimeError(msg)
    if member.issym():
        _validate_symlink_target(member_path, member.linkname, member.name)
        symlink_paths.add(member_path)


def _strip_first_path_component(path: str) -> str | None:
    pure_path = PurePosixPath(path)
    if pure_path.is_absolute():
        msg = f"archive entry path must be relative: {path!r}"
        raise RuntimeError(msg)

    parts = pure_path.parts
    if not parts:
        return None

    stripped_parts = parts[1:]
    if not stripped_parts:
        return None

    if any(part in {"", ".", ".."} for part in stripped_parts):
        msg = f"archive entry path is invalid after stripping root: {path!r}"
        raise RuntimeError(msg)

    return str(PurePosixPath(*stripped_parts))
