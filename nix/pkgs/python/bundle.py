# noqa: INP001

from __future__ import annotations

import pathlib
import py_compile
import sys
import tempfile
import zipfile
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Iterator


def python_sources(
    source: pathlib.Path,
    archive_path: pathlib.PurePosixPath,
) -> Iterator[tuple[pathlib.Path, pathlib.PurePosixPath]]:
    if source.is_file():
        if source.suffix == ".py":
            yield source, archive_path
        return

    init = source / "__init__.py"
    if not init.is_file():
        return

    yield init, archive_path / "__init__.py"
    for child in sorted(source.iterdir()):
        if child == init:
            continue
        if child.is_file() and child.suffix == ".py":
            yield child, archive_path / child.name
        elif child.is_dir():
            yield from python_sources(child, archive_path / child.name)


def top_level_python_sources(
    root: pathlib.Path,
    *,
    exclude_wheel_metadata: bool = False,
) -> Iterator[tuple[pathlib.Path, pathlib.PurePosixPath]]:
    for item in sorted(root.iterdir()):
        if exclude_wheel_metadata and item.name.endswith((".dist-info", ".data")):
            continue
        if item.is_dir() and not (item / "__init__.py").is_file():
            for child in sorted(item.iterdir()):
                if child.is_file() and child.suffix == ".py":
                    yield child, pathlib.PurePosixPath(child.name)
        else:
            yield from python_sources(item, pathlib.PurePosixPath(item.name))


def write_bytecode(
    archive: zipfile.ZipFile,
    source: pathlib.Path,
    archive_source: pathlib.PurePosixPath,
    compiled_path: pathlib.Path,
) -> None:
    py_compile.compile(
        source,
        cfile=compiled_path,
        dfile=archive_source.as_posix(),
        doraise=True,
        # The bundle is immutable; avoid hashing source files at import time.
        invalidation_mode=py_compile.PycInvalidationMode.UNCHECKED_HASH,
    )
    archive_name = archive_source.with_suffix(".pyc").as_posix()
    info = zipfile.ZipInfo(archive_name, date_time=(1980, 1, 1, 0, 0, 0))
    info.compress_type = zipfile.ZIP_DEFLATED
    info.create_system = 3
    info.external_attr = 0o100644 << 16
    archive.writestr(info, compiled_path.read_bytes(), compresslevel=6)


if __name__ == "__main__":
    pyzip_path = sys.argv[1] + ".zip"
    source_paths = sys.argv[2:]

    with (
        zipfile.ZipFile(
            pyzip_path,
            "w",
            compression=zipfile.ZIP_DEFLATED,
            compresslevel=6,
            strict_timestamps=False,
        ) as archive,
        tempfile.TemporaryDirectory() as compile_dir,
    ):
        compiled_path = pathlib.Path(compile_dir) / "module.pyc"
        for path_str in source_paths:
            path = pathlib.Path(path_str)

            if path.is_file() and path.suffix == ".whl":
                with tempfile.TemporaryDirectory() as tmpdir:
                    with zipfile.ZipFile(path, "r") as whl:
                        whl.extractall(tmpdir)
                    root = pathlib.Path(tmpdir)
                    for source, archive_source in top_level_python_sources(
                        root,
                        exclude_wheel_metadata=True,
                    ):
                        write_bytecode(
                            archive,
                            source,
                            archive_source,
                            compiled_path,
                        )
            elif path.is_dir():
                for source, archive_source in top_level_python_sources(path):
                    write_bytecode(
                        archive,
                        source,
                        archive_source,
                        compiled_path,
                    )
