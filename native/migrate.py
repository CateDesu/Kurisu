#!/usr/bin/env python3
"""Copy a stopped Kurisu profile into a new native profile."""

import argparse
import ctypes
from contextlib import closing
import hashlib
import os
from pathlib import Path
import shutil
import sqlite3
import stat
import struct
import sys
import tempfile
import json
import math


PREFERENCE_KEYS = (
    "kurisu.list.sort",
    "kurisu.list.dir",
    "kurisu.list.filter",
    "kurisu.list.q",
    "kurisu.list.synced",
    "kurisu.cal.mine",
    "kurisu.torrents.new",
    "kurisu.torrents.statuses",
    "kurisu.torrents.filters",
)
FORMAT = "kurisu-ui-preferences-v1"
MARKER = b"kurisu-native-v1\n"
MAX_PREFERENCES_SIZE = 1024 * 1024


class TransferError(Exception):
    pass


def _forbidden(path: Path) -> bool:
    return any("hentai" in part.casefold() for part in path.parts)


def _link_or_reparse(metadata: os.stat_result) -> bool:
    return stat.S_ISLNK(metadata.st_mode) or bool(
        getattr(metadata, "st_file_attributes", 0) & 0x400
    )


def _check_path(path: Path) -> None:
    if not path.is_absolute() or ".." in path.parts or _forbidden(path):
        raise TransferError("Use an absolute permitted local path without parent traversal")
    if os.name == "nt" and str(path).startswith("\\\\"):
        raise TransferError("Network paths are not supported")
    current = Path(path.anchor)
    for part in path.parts[1:]:
        current /= part
        try:
            metadata = current.lstat()
        except FileNotFoundError:
            break
        if _link_or_reparse(metadata):
            raise TransferError(f"Links and reparse points are not allowed: {current}")


def _regular_file(path: Path) -> None:
    _check_path(path)
    try:
        mode = path.lstat().st_mode
    except FileNotFoundError as error:
        raise TransferError(f"File does not exist: {path}") from error
    if not stat.S_ISREG(mode):
        raise TransferError(f"Expected a regular file: {path}")


def _new_path(path: Path) -> None:
    _check_path(path)
    if os.path.lexists(path):
        raise TransferError(f"Destination already exists: {path}")
    if not path.parent.is_dir():
        raise TransferError(f"Create the destination parent first: {path.parent}")


def _within(path: Path, parent: Path) -> bool:
    return path == parent or parent in path.parents


def _load_preferences(path: Path) -> dict[str, str | None]:
    _regular_file(path)
    if path.stat().st_size > MAX_PREFERENCES_SIZE:
        raise TransferError("Preference export is too large")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, ValueError) as error:
        raise TransferError("Preference export is not valid UTF-8 JSON") from error
    if not isinstance(data, dict) or set(data) != {"format", "preferences"}:
        raise TransferError("Preference export has an unsupported format")
    values = data["preferences"]
    if data["format"] != FORMAT or not isinstance(values, dict):
        raise TransferError("Preference export has an unsupported format")
    if set(values) != set(PREFERENCE_KEYS):
        raise TransferError("Preference export must contain exactly the nine UI keys")
    if any(value is not None and not isinstance(value, str) for value in values.values()):
        raise TransferError("Preference values must be strings or null")
    return values


def _native_preference_value(key: str, value: str | None) -> str | None:
    if key != "kurisu.list.synced" or value is None:
        return value
    try:
        milliseconds = float(value)
    except ValueError:
        return None
    if not math.isfinite(milliseconds) or milliseconds <= 0:
        return None
    seconds = math.floor(milliseconds / 1000)
    if seconds > 2**63 - 1:
        return None
    return str(seconds)


def _source_files(source: Path) -> list[Path]:
    _regular_file(source)
    if source.name.casefold() != "kurisu.db":
        raise TransferError("Source must be the legacy kurisu.db shown in Settings")
    if os.path.lexists(source.parent / ".kurisu-native-profile"):
        raise TransferError("Source is already a native profile")
    if os.path.lexists(Path(f"{source}-journal")):
        raise TransferError("Source has a rollback journal; start and stop legacy Kurisu first")
    files = [source]
    for suffix in ("-wal", "-shm"):
        sidecar = Path(f"{source}{suffix}")
        if os.path.lexists(sidecar):
            _regular_file(sidecar)
            files.append(sidecar)
    return files


def _fingerprints(files: list[Path]) -> dict[str, bytes]:
    result = {}
    for path in files:
        digest = hashlib.sha256()
        with path.open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(block)
        result[str(path)] = digest.digest()
    return result


def _copy_private(source: Path, destination: Path) -> None:
    fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with source.open("rb") as input_stream, os.fdopen(fd, "wb") as output_stream:
            fd = -1
            shutil.copyfileobj(input_stream, output_stream)
            output_stream.flush()
            os.fsync(output_stream.fileno())
    finally:
        if fd >= 0:
            os.close(fd)
    os.chmod(destination, 0o600)


def _validate_schema(connection: sqlite3.Connection) -> int:
    integrity = connection.execute("PRAGMA integrity_check").fetchall()
    if integrity != [("ok",)]:
        raise TransferError("Database integrity check failed")
    version = connection.execute("PRAGMA user_version").fetchone()[0]
    if not 1 <= version <= 7:
        raise TransferError(f"Unsupported legacy schema version: {version}")
    columns = {
        "settings": {"key", "value"},
        "media": {
            "id", "id_mal", "title_romaji", "title_english", "title_native",
            "cover_medium", "cover_large", "episodes", "format", "status",
            "average_score", "season", "season_year", "description", "cached_at",
        },
        "list_entry": {"media_id", "entry_id", "status", "progress", "score", "repeat", "updated_at"},
        "watched_file": {"path", "media_id", "episode", "watched_at"},
    }
    if version >= 2:
        columns["media"].update(("next_airing_episode", "next_airing_at"))
    if version >= 3:
        columns["media"].update(("banner_image", "genres", "duration", "source", "studios"))
    if version >= 4:
        columns["rss_seen"] = {"guid", "seen_at"}
    if version >= 5:
        columns["pending_change"] = {"account_id", "media_id", "payload"}
        columns["watch_history"] = {"id", "account_id", "path", "media_id", "episode", "watched_at"}
    present = {
        name for (name,) in connection.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table'"
        )
    }
    if not columns.keys() <= present:
        raise TransferError("Database schema is missing required tables")
    for table in columns:
        found = {row[1] for row in connection.execute(f"PRAGMA table_info({table})")}
        if not columns[table] <= found:
            raise TransferError(f"Database schema is missing {table} columns")
    indexes = set()
    if version >= 6:
        indexes.add("watch_history_duplicate")
    if version >= 7:
        indexes.add("watch_history_account_id")
    if indexes:
        found = {
            name for (name,) in connection.execute(
                "SELECT name FROM sqlite_master WHERE type = 'index'"
            )
        }
        if not indexes <= found:
            raise TransferError("Database schema is missing required indexes")
    return version


def _wal_checksum(data: bytes, order: str, previous: tuple[int, int]) -> tuple[int, int]:
    first, second = previous
    for left, right in struct.iter_unpack(order + "II", data):
        first = (first + left + second) & 0xffffffff
        second = (second + right + first) & 0xffffffff
    return first, second


def _validate_wal(database: Path, wal: Path) -> None:
    def refuse(reason: str) -> None:
        raise TransferError(
            f"WAL {reason}; preserve the original files and verify a recovered copy before transfer"
        )

    with wal.open("rb") as stream:
        header = stream.read(32)
        if not header:
            return
        if len(header) != 32:
            refuse("header is incomplete")
        magic, version, page_size, _, salt1, salt2, sum1, sum2 = struct.unpack(">8I", header)
        if magic not in (0x377f0682, 0x377f0683) or version != 3007000:
            refuse("header is invalid or unsupported")
        if not 512 <= page_size <= 65536 or page_size & (page_size - 1):
            refuse("page size is invalid")
        with database.open("rb") as main:
            main_header = main.read(18)
        main_size = int.from_bytes(main_header[16:18], "big")
        if page_size != (65536 if main_size == 1 else main_size):
            refuse("page size differs from the database")
        order = "<" if magic == 0x377f0682 else ">"
        checksum = _wal_checksum(header[:24], order, (0, 0))
        if checksum != (sum1, sum2):
            refuse("header checksum failed")
        while frame := stream.read(24 + page_size):
            if len(frame) != 24 + page_size:
                refuse("frame is incomplete")
            page, size, frame_salt1, frame_salt2, sum1, sum2 = struct.unpack(">6I", frame[:24])
            if (frame_salt1, frame_salt2) != (salt1, salt2):
                # Reset leftovers and damaged salts cannot be distinguished safely here.
                refuse("contains an inactive or damaged tail")
            if not 1 <= page <= 0xfffffffe or size > 0xfffffffe:
                refuse("frame page number is invalid")
            checksum = _wal_checksum(frame[:8], order, checksum)
            checksum = _wal_checksum(frame[24:], order, checksum)
            if checksum != (sum1, sum2):
                refuse("frame checksum failed")


def _snapshot(source: Path, files: list[Path], stage: Path, backup: Path) -> dict[str, bytes]:
    before = _fingerprints(files)
    copied = stage / "kurisu.db"
    _copy_private(source, copied)
    if Path(f"{source}-wal") in files:
        _copy_private(Path(f"{source}-wal"), Path(f"{copied}-wal"))
    if before != _fingerprints(_source_files(source)):
        raise TransferError("Source changed during transfer; stop both programs and retry")
    if Path(f"{source}-wal") in files:
        _validate_wal(copied, Path(f"{copied}-wal"))
    uri = copied.as_uri() + "?mode=ro"
    with closing(sqlite3.connect(uri, uri=True)) as source_db:
        _validate_schema(source_db)
        with closing(sqlite3.connect(backup)) as backup_db:
            source_db.backup(backup_db)
    with closing(sqlite3.connect(backup)) as backup_db:
        backup_db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        backup_db.execute("PRAGMA journal_mode=DELETE")
        _validate_schema(backup_db)
    if before != _fingerprints(_source_files(source)):
        raise TransferError("Source changed during transfer; stop both programs and retry")
    os.chmod(backup, 0o600)
    for suffix in ("-wal", "-shm"):
        if os.path.lexists(Path(f"{backup}{suffix}")):
            raise TransferError("Backup still has SQLite sidecars")
    return before


def _populate_native(backup: Path, stage: Path, values: dict[str, str | None]) -> None:
    database = stage / "kurisu.db"
    _copy_private(backup, database)
    with closing(sqlite3.connect(database)) as connection:
        connection.execute("PRAGMA journal_mode=DELETE")
        _validate_schema(connection)
        with connection:
            for key in PREFERENCE_KEYS:
                stored = _native_preference_value(key, values[key])
                database_key = f"native_preference:{key}"
                if stored is None:
                    connection.execute("DELETE FROM settings WHERE key = ?", (database_key,))
                else:
                    connection.execute(
                        "INSERT OR REPLACE INTO settings(key, value) VALUES (?, ?)",
                        (database_key, stored),
                    )
        _validate_schema(connection)
        keys = tuple(f"native_preference:{key}" for key in PREFERENCE_KEYS)
        placeholders = ",".join("?" for _ in keys)
        actual = dict(connection.execute(
            f"SELECT key, value FROM settings WHERE key IN ({placeholders})", keys
        ))
        if actual != {
            f"native_preference:{key}": converted
            for key, value in values.items()
            if (converted := _native_preference_value(key, value)) is not None
        }:
            raise TransferError("Native preference verification failed")
    for suffix in ("-wal", "-shm"):
        if os.path.lexists(Path(f"{database}{suffix}")):
            raise TransferError("Native profile still has SQLite sidecars")
    marker = stage / ".kurisu-native-profile"
    fd = os.open(marker, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream:
        stream.write(MARKER)
        stream.flush()
        os.fsync(stream.fileno())


def _publish_dir_no_replace(stage: Path, destination: Path) -> None:
    if sys.platform.startswith("linux"):
        libc = ctypes.CDLL(None, use_errno=True)
        rename = getattr(libc, "renameat2", None)
        if rename is None:
            raise TransferError("This Linux system lacks atomic no-replace rename")
        rename.argtypes = (ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint)
        rename.restype = ctypes.c_int
        if rename(-100, os.fsencode(stage), -100, os.fsencode(destination), 1) != 0:
            error = ctypes.get_errno()
            raise TransferError(f"Could not publish new profile: {os.strerror(error)}")
    elif os.name == "nt":
        os.rename(stage, destination)
    else:
        raise TransferError("Profile transfer supports Linux and Windows only")


def transfer(source: Path, preferences: Path, backup: Path, destination: Path) -> None:
    for path in (source, preferences, backup, destination):
        _check_path(path)
    if os.name == "nt":
        local_data = os.environ.get("LOCALAPPDATA")
        if not local_data:
            raise TransferError("Windows LOCALAPPDATA is unavailable")
        private_root = Path(local_data)
        _check_path(private_root)
        if not _within(backup, private_root) or not _within(destination, private_root):
            raise TransferError("Place backup and native profile under LOCALAPPDATA")
    if _within(destination, source.parent) or _within(backup, source.parent):
        raise TransferError("Backup and native profile must be outside the legacy profile")
    if _within(backup, destination) or _within(preferences, destination):
        raise TransferError("Inputs and backup must be outside the new native profile")
    if len({source, preferences, backup, destination}) != 4:
        raise TransferError("Source, export, backup and destination must be distinct")
    files = _source_files(source)
    values = _load_preferences(preferences)
    _new_path(backup)
    for suffix in ("-wal", "-shm"):
        _new_path(Path(f"{backup}{suffix}"))
    _new_path(destination)

    with tempfile.TemporaryDirectory(prefix=".kurisu-transfer-", dir=backup.parent) as scratch:
        scratch_path = Path(scratch)
        snapshot = scratch_path / "source"
        snapshot.mkdir(mode=0o700)
        prepared_backup = scratch_path / "backup.db"
        try:
            original = _snapshot(source, files, snapshot, prepared_backup)
        except sqlite3.DatabaseError as error:
            raise TransferError("Source database could not be validated") from error
        try:
            os.link(prepared_backup, backup)
        except FileExistsError as error:
            raise TransferError("Backup destination appeared during transfer") from error
        backup_digest = _fingerprints([backup])
        stage = Path(tempfile.mkdtemp(prefix=".kurisu-native-transfer-", dir=destination.parent))
        try:
            _populate_native(backup, stage, values)
            if backup_digest != _fingerprints([backup]):
                raise TransferError("Backup changed during transfer")
            if original != _fingerprints(_source_files(source)):
                raise TransferError("Source changed during transfer")
            _publish_dir_no_replace(stage, destination)
        finally:
            if stage.exists():
                shutil.rmtree(stage)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-db", type=Path, required=True)
    parser.add_argument("--preferences", type=Path, required=True)
    parser.add_argument("--backup-db", type=Path, required=True)
    parser.add_argument("--destination", type=Path, required=True)
    args = parser.parse_args()
    try:
        transfer(args.source_db, args.preferences, args.backup_db, args.destination)
    except (TransferError, OSError) as error:
        print(f"Transfer stopped: {error}", file=sys.stderr)
        return 1
    print(f"Native profile ready: {args.destination}")
    print(f"Standalone rollback backup: {args.backup_db}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
