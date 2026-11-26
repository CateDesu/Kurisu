import hashlib
from contextlib import closing
import errno
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sqlite3
import stat
import struct
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("native_migrate", Path(__file__).with_name("migrate.py"))
MIGRATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MIGRATE)


def schema(connection, version=7):
    connection.executescript("""
        CREATE TABLE media (id INTEGER PRIMARY KEY, id_mal INTEGER, title_romaji TEXT,
            title_english TEXT, title_native TEXT, cover_medium TEXT, cover_large TEXT,
            episodes INTEGER, format TEXT, status TEXT, average_score INTEGER,
            season TEXT, season_year INTEGER, description TEXT, cached_at INTEGER NOT NULL);
        CREATE TABLE list_entry (media_id INTEGER PRIMARY KEY, entry_id INTEGER, status TEXT NOT NULL,
            progress INTEGER NOT NULL DEFAULT 0, score REAL, repeat INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER);
        CREATE TABLE watched_file (path TEXT PRIMARY KEY, media_id INTEGER NOT NULL,
            episode INTEGER NOT NULL, watched_at INTEGER NOT NULL);
        CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    """)
    if version >= 2:
        connection.execute("ALTER TABLE media ADD COLUMN next_airing_episode INTEGER")
        connection.execute("ALTER TABLE media ADD COLUMN next_airing_at INTEGER")
    if version >= 3:
        for column, kind in (("banner_image", "TEXT"), ("genres", "TEXT"),
                             ("duration", "INTEGER"), ("source", "TEXT"), ("studios", "TEXT")):
            connection.execute(f"ALTER TABLE media ADD COLUMN {column} {kind}")
    if version >= 4:
        connection.execute("CREATE TABLE rss_seen (guid TEXT PRIMARY KEY, seen_at INTEGER NOT NULL)")
    if version >= 5:
        connection.executescript("""
            CREATE TABLE pending_change (account_id INTEGER NOT NULL, media_id INTEGER NOT NULL,
                payload TEXT NOT NULL, PRIMARY KEY(account_id, media_id));
            CREATE TABLE watch_history (id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id INTEGER NOT NULL, path TEXT NOT NULL, media_id INTEGER NOT NULL,
                episode INTEGER NOT NULL, watched_at INTEGER NOT NULL);
        """)
    if version >= 6:
        connection.execute("CREATE INDEX watch_history_duplicate ON watch_history (account_id,path,media_id,episode,watched_at)")
    if version >= 7:
        connection.execute("CREATE INDEX watch_history_account_id ON watch_history (account_id,id)")
    if version >= 8:
        connection.execute("ALTER TABLE list_entry ADD COLUMN queued_creation TEXT")
    connection.execute(f"PRAGMA user_version = {version}")
    connection.commit()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class TransferTests(unittest.TestCase):
    def setUp(self):
        private_parent = os.environ.get("LOCALAPPDATA") if os.name == "nt" else None
        self.directory = tempfile.TemporaryDirectory(prefix="kurisu-transfer-test-", dir=private_parent)
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.legacy = self.root / "legacy"
        self.legacy.mkdir()
        self.source = self.legacy / "kurisu.db"
        self.export = self.root / "preferences.json"
        self.backup = self.root / "backup.db"
        self.native = self.root / "native-profile"
        self.values = {key: None for key in MIGRATE.PREFERENCE_KEYS}
        self.values.update({
            "kurisu.list.sort": "title",
            "kurisu.list.dir": "asc",
            "kurisu.list.filter": "",
            "kurisu.list.q": "Synthetic query",
            "kurisu.list.synced": "1793910000123",
            "kurisu.cal.mine": "0",
            "kurisu.torrents.new": "1",
            "kurisu.torrents.statuses": '["CURRENT","REPEATING"]',
            "kurisu.torrents.filters": '{"category":"all","release":"any"}',
        })
        self.write_export()

    def write_export(self):
        self.export.write_text(json.dumps({
            "format": MIGRATE.FORMAT, "preferences": self.values,
        }), encoding="utf-8")

    def make_source(self, version=7):
        with closing(sqlite3.connect(self.source)) as connection:
            schema(connection, version)
            connection.execute("INSERT INTO media(id, title_english, cached_at) VALUES (3, 'Synthetic', 1)")
            connection.execute("INSERT INTO list_entry(media_id, entry_id, status, progress) VALUES (3, 33, 'CURRENT', 2)")
            connection.execute("INSERT INTO settings(key, value) VALUES ('anilist_token', 'synthetic-session')")
            connection.execute("INSERT INTO settings(key, value) VALUES ('anilist_user_id', '47')")
            connection.execute("INSERT INTO settings(key, value) VALUES ('library_bindings', ?)",
                               (json.dumps({"/synthetic/show": {"media_id": 3, "episode_offset": -12}}),))
            connection.execute("INSERT INTO settings(key, value) VALUES ('library_folders', ?)",
                               (json.dumps(["/synthetic/library"]),))
            connection.execute("INSERT INTO settings(key, value) VALUES ('tracking_mode', 'prompt')")
            connection.execute("INSERT INTO settings(key, value) VALUES ('native_preference:kurisu.future', 'synthetic-future')")
            if version >= 5:
                connection.execute("INSERT INTO pending_change VALUES (47, 3, ?)",
                                   (json.dumps({"synthetic": "pending"}),))
                connection.execute("INSERT INTO watch_history(account_id,path,media_id,episode,watched_at) VALUES (47,'/synthetic/ep2.mkv',3,2,3)")
            connection.commit()

    def run_cli(self):
        return subprocess.run([
            sys.executable, str(Path(MIGRATE.__file__)),
            "--source-db", str(self.source),
            "--preferences", str(self.export),
            "--backup-db", str(self.backup),
            "--destination", str(self.native),
        ], text=True, capture_output=True, check=False)

    def make_wal(self, mode="committed"):
        self.make_source()
        child = """
import os, sqlite3, sys
db = sqlite3.connect(sys.argv[1])
mode = sys.argv[2]
db.execute('PRAGMA journal_mode=WAL')
db.execute('PRAGMA wal_autocheckpoint=0')
db.execute('UPDATE list_entry SET progress=4 WHERE media_id=3')
if mode in ('padded', 'reset', 'reset-uncommitted'):
    db.executemany('INSERT INTO settings VALUES (?,?)',
                   [(f'large-{n}', 'x' * 4000) for n in range(100)])
db.commit()
if mode in ('reset', 'reset-uncommitted'):
    busy, frames, checkpointed = db.execute('PRAGMA wal_checkpoint(RESTART)').fetchone()
    assert busy == 0 and frames == checkpointed
if mode == 'reset':
    db.execute('UPDATE list_entry SET progress=5 WHERE media_id=3')
    db.commit()
elif mode == 'uncommitted':
    db.execute('PRAGMA cache_size=1')
    db.execute('UPDATE list_entry SET progress=9 WHERE media_id=3')
    db.executemany('INSERT INTO settings VALUES (?,?)',
                   [(f'uncommitted-{n}', 'x' * 4000) for n in range(100)])
elif mode == 'reset-uncommitted':
    db.execute('PRAGMA cache_size=1')
    db.execute('UPDATE list_entry SET progress=9 WHERE media_id=3')
    db.execute("UPDATE settings SET value=replace(value,'x','y') "
               "WHERE key LIKE 'large-%' AND CAST(substr(key,7) AS INTEGER)<50")
os._exit(0)
"""
        subprocess.run([sys.executable, "-c", child, str(self.source), mode], check=True)
        return Path(f"{self.source}-wal")

    def assert_wal_refused(self):
        before = {str(path): digest(path) for path in MIGRATE._source_files(self.source)}
        result = self.run_cli()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("WAL", result.stderr)
        self.assertIn("verify a recovered copy", result.stderr)
        self.assertFalse(self.backup.exists())
        self.assertFalse(self.native.exists())
        self.assertEqual(before, {str(path): digest(path) for path in MIGRATE._source_files(self.source)})
        self.assertEqual(list(self.root.glob(".kurisu-*")), [])

    def test_invalid_wal_headers_refuse_publication(self):
        wal = self.make_wal()
        original = wal.read_bytes()
        variants = {
            "short": original[:16],
            "magic": b"bad!" + original[4:],
            "version": original[:4] + bytes(4) + original[8:],
            "page-size": original[:8] + (513).to_bytes(4, "big") + original[12:],
            "checksum": original[:24] + bytes(8) + original[32:],
        }
        for name, content in variants.items():
            with self.subTest(name=name):
                wal.write_bytes(content)
                self.assert_wal_refused()

    def test_truncated_main_database_header_refuses_wal_validation(self):
        wal = self.make_wal()
        header = bytearray(struct.pack(">6I", 0x377f0682, 3007000, 65536, 0, 1, 2))
        header.extend(struct.pack(">2I", *MIGRATE._wal_checksum(bytes(header), "<", (0, 0))))
        wal.write_bytes(bytes(header))
        self.source.write_bytes(b"SQLite format 3\x00\x01")
        with self.assertRaisesRegex(MIGRATE.TransferError, "database header is incomplete"):
            MIGRATE._validate_wal(self.source, wal)
        self.assert_wal_refused()

    def test_damaged_or_incomplete_wal_frames_refuse_publication(self):
        wal = self.make_wal()
        original = wal.read_bytes()
        variants = {
            "checksum": original[:-1] + bytes([original[-1] ^ 1]),
            "truncated-commit": original[:-100],
            "partial-tail": original + bytes(12),
            "salt": original[:40] + bytes([original[40] ^ 1]) + original[41:],
        }
        for name, content in variants.items():
            with self.subTest(name=name):
                wal.write_bytes(content)
                self.assert_wal_refused()

    def test_empty_wal_keeps_main_database(self):
        self.make_source()
        Path(f"{self.source}-wal").write_bytes(b"")
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stderr)
        with closing(sqlite3.connect(self.native / "kurisu.db")) as connection:
            self.assertEqual(connection.execute("SELECT progress FROM list_entry").fetchone(), (2,))

    def test_valid_uncommitted_wal_frames_are_not_imported(self):
        wal = self.make_wal("uncommitted")
        content = wal.read_bytes()
        frame_size = 24 + int.from_bytes(content[8:12], "big")
        frames = [content[offset:offset + frame_size] for offset in range(32, len(content), frame_size)]
        commits = [i for i, frame in enumerate(frames) if int.from_bytes(frame[4:8], "big")]
        self.assertTrue(commits)
        self.assertGreater(len(frames) - 1, commits[-1], "Fixture must spill uncommitted frames")
        before = {str(path): digest(path) for path in MIGRATE._source_files(self.source)}
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stderr)
        for database in (self.backup, self.native / "kurisu.db"):
            with closing(sqlite3.connect(database)) as connection:
                self.assertEqual(connection.execute("SELECT progress FROM list_entry").fetchone(), (4,))
                self.assertEqual(connection.execute("SELECT count(*) FROM settings WHERE key LIKE 'uncommitted-%'").fetchone(), (0,))
        self.assertEqual(before, {str(path): digest(path) for path in MIGRATE._source_files(self.source)})

    def test_reset_tail_keeps_latest_commit_without_rewriting_source(self):
        wal = self.make_wal("reset")
        content = wal.read_bytes()
        frame_size = 24 + int.from_bytes(content[8:12], "big")
        self.assertTrue(any(content[offset + 8:offset + 16] != content[16:24]
                            for offset in range(32, len(content), frame_size)))
        before = {str(path): digest(path) for path in MIGRATE._source_files(self.source)}
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stderr)
        for database in (self.backup, self.native / "kurisu.db"):
            with closing(sqlite3.connect(database)) as connection:
                self.assertEqual(connection.execute("SELECT progress FROM list_entry").fetchone(), (5,))
        self.assertEqual(before, {str(path): digest(path) for path in MIGRATE._source_files(self.source)})

    def test_recycled_wal_accepts_an_older_checksum_byte_order(self):
        wal = self.make_wal("padded")
        content = bytearray(wal.read_bytes())
        native_magic = content[3]
        content[3] ^= 1
        order = ">" if content[3] == 0x83 else "<"
        frame_size = 24 + int.from_bytes(content[8:12], "big")
        checksum = MIGRATE._wal_checksum(content[:24], order, (0, 0))
        struct.pack_into(">2I", content, 24, *checksum)
        for offset in range(32, len(content), frame_size):
            checksum = MIGRATE._wal_checksum(content[offset:offset + 8], order, checksum)
            checksum = MIGRATE._wal_checksum(content[offset + 24:offset + frame_size], order, checksum)
            struct.pack_into(">2I", content, offset + 16, *checksum)
        wal.write_bytes(content)
        Path(f"{self.source}-shm").unlink()

        child = """
import os, sqlite3, sys
db = sqlite3.connect(sys.argv[1])
busy, frames, checkpointed = db.execute('PRAGMA wal_checkpoint(RESTART)').fetchone()
assert busy == 0 and frames == checkpointed and frames > 2
db.execute('UPDATE list_entry SET progress=5 WHERE media_id=3')
db.commit()
os._exit(0)
"""
        subprocess.run([sys.executable, "-c", child, str(self.source)], check=True)
        recycled = wal.read_bytes()
        self.assertEqual(recycled[3], native_magic)
        self.assertEqual(recycled[32 + 2 * frame_size:], content[32 + 2 * frame_size:])

        def check_rows(database):
            with closing(sqlite3.connect(database)) as connection:
                self.assertEqual(connection.execute("PRAGMA integrity_check").fetchall(), [("ok",)])
                self.assertEqual(connection.execute("SELECT progress FROM list_entry").fetchone(), (5,))
                self.assertEqual(connection.execute(
                    "SELECT count(*) FROM settings WHERE key LIKE 'large-%'"
                ).fetchone(), (100,))

        control = self.root / "opposite-order-control.db"
        shutil.copyfile(self.source, control)
        shutil.copyfile(wal, Path(f"{control}-wal"))
        check_rows(control)
        before = {str(path): digest(path) for path in MIGRATE._source_files(self.source)}
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stderr)
        for database in (self.backup, self.native / "kurisu.db"):
            check_rows(database)
        self.assertEqual(before, {str(path): digest(path) for path in MIGRATE._source_files(self.source)})

    def test_recycled_wal_without_a_new_commit_keeps_checkpointed_rows(self):
        wal = self.make_wal("reset-uncommitted")
        content = wal.read_bytes()
        frame_size = 24 + int.from_bytes(content[8:12], "big")
        frames = [content[offset:offset + frame_size] for offset in range(32, len(content), frame_size)]
        current = [frame for frame in frames if frame[8:16] == content[16:24]]
        self.assertGreater(len(current), 1)
        self.assertLess(len(current), len(frames))
        self.assertTrue(all(frame[4:8] == bytes(4) for frame in current))

        def check_rows(database):
            with closing(sqlite3.connect(database)) as connection:
                self.assertEqual(connection.execute("PRAGMA integrity_check").fetchall(), [("ok",)])
                self.assertEqual(connection.execute("SELECT progress FROM list_entry").fetchone(), (4,))
                self.assertEqual(connection.execute(
                    "SELECT count(*),min(value),max(value) FROM settings WHERE key LIKE 'large-%'"
                ).fetchone(), (100, "x" * 4000, "x" * 4000))

        control = self.root / "sqlite-control.db"
        shutil.copyfile(self.source, control)
        shutil.copyfile(wal, Path(f"{control}-wal"))
        check_rows(control)

        second = 32 + frame_size
        stale = 32 + len(current) * frame_size
        wal.write_bytes(content[:second + 8] + content[stale + 8:stale + 16]
                        + content[second + 16:second + frame_size])
        self.assert_wal_refused()
        wal.write_bytes(content)

        before = {str(path): digest(path) for path in MIGRATE._source_files(self.source)}
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stderr)
        for database in (self.backup, self.native / "kurisu.db"):
            check_rows(database)
        self.assertEqual(before, {str(path): digest(path) for path in MIGRATE._source_files(self.source)})

    def test_documented_cli_carries_wal_rows_preferences_and_rollback(self):
        self.make_source()
        child = """
import os, sqlite3, sys
db = sqlite3.connect(sys.argv[1])
db.execute('PRAGMA journal_mode=WAL')
db.execute('PRAGMA wal_autocheckpoint=0')
db.execute("UPDATE list_entry SET progress=4 WHERE media_id=3")
db.execute("UPDATE settings SET value='wal-setting' WHERE key='tracking_mode'")
db.commit()
os._exit(0)
"""
        subprocess.run([sys.executable, "-c", child, str(self.source)], check=True)
        wal = Path(f"{self.source}-wal")
        self.assertTrue(wal.exists(), "Fixture requires committed uncheckpointed WAL")
        original = {str(path): digest(path) for path in MIGRATE._source_files(self.source)}
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.native / ".kurisu-native-profile").read_bytes(), MIGRATE.MARKER)
        self.assertEqual(original, {str(path): digest(path) for path in MIGRATE._source_files(self.source)})
        backup_hash = digest(self.backup)
        for database in (self.backup, self.native / "kurisu.db"):
            with closing(sqlite3.connect(database)) as connection:
                self.assertEqual(connection.execute("SELECT progress FROM list_entry WHERE media_id=3").fetchone(), (4,))
                self.assertEqual(connection.execute("SELECT value FROM settings WHERE key='tracking_mode'").fetchone(), ("wal-setting",))
                self.assertEqual(connection.execute("SELECT account_id,media_id FROM pending_change").fetchone(), (47, 3))
                self.assertEqual(connection.execute("SELECT account_id,media_id,episode FROM watch_history").fetchone(), (47, 3, 2))
                self.assertEqual(connection.execute("SELECT value FROM settings WHERE key='anilist_token'").fetchone(), ("synthetic-session",))
                self.assertEqual(connection.execute("SELECT value FROM settings WHERE key='native_preference:kurisu.future'").fetchone(), ("synthetic-future",))
                if database != self.backup:
                    self.assertEqual(connection.execute("SELECT value FROM settings WHERE key='native_preference:kurisu.list.synced'").fetchone(), ("1793910000",))
                self.assertIn('"episode_offset": -12', connection.execute("SELECT value FROM settings WHERE key='library_bindings'").fetchone()[0])
                self.assertEqual(json.loads(connection.execute("SELECT value FROM settings WHERE key='library_folders'").fetchone()[0]), ["/synthetic/library"])
                keys = tuple("native_preference:" + key for key in MIGRATE.PREFERENCE_KEYS)
                placeholders = ",".join("?" for _ in keys)
                found = dict(connection.execute(
                    f"SELECT key, value FROM settings WHERE key IN ({placeholders})", keys
                ))
                if database == self.backup:
                    self.assertEqual(found, {})
                else:
                    self.assertEqual(found, {
                        "native_preference:" + key: converted
                        for key, value in self.values.items()
                        if (converted := MIGRATE._native_preference_value(key, value)) is not None
                    })
        with closing(sqlite3.connect(self.native / "kurisu.db")) as connection:
            connection.execute("UPDATE list_entry SET progress=9 WHERE media_id=3")
            connection.commit()
        self.assertEqual(digest(self.backup), backup_hash)
        self.assertEqual(original, {str(path): digest(path) for path in MIGRATE._source_files(self.source)})
        with closing(sqlite3.connect(self.source)) as connection:
            self.assertEqual(connection.execute("SELECT progress FROM list_entry WHERE media_id=3").fetchone(), (4,))
        if os.name != "nt":
            for path, expected in ((self.native, 0o700), (self.native / "kurisu.db", 0o600),
                                   (self.native / ".kurisu-native-profile", 0o600), (self.backup, 0o600)):
                self.assertEqual(path.stat().st_mode & 0o777, expected)

    def test_absent_and_empty_preferences_remain_distinct(self):
        self.make_source()
        self.values["kurisu.list.sort"] = None
        self.values["kurisu.list.q"] = ""
        self.write_export()
        MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        with closing(sqlite3.connect(self.native / "kurisu.db")) as connection:
            self.assertIsNone(connection.execute("SELECT value FROM settings WHERE key='native_preference:kurisu.list.sort'").fetchone())
            self.assertEqual(connection.execute("SELECT value FROM settings WHERE key='native_preference:kurisu.list.q'").fetchone(), ("",))

    def test_invalid_legacy_sync_time_uses_native_default(self):
        self.make_source()
        for number, value in enumerate((None, "", "invalid", "0", "-1", "1_000", "Infinity", "NaN")):
            with self.subTest(value=value):
                self.values["kurisu.list.synced"] = value
                self.write_export()
                backup = self.root / f"backup-invalid-{number}.db"
                native = self.root / f"native-invalid-{number}"
                MIGRATE.transfer(self.source, self.export, backup, native)
                with closing(sqlite3.connect(native / "kurisu.db")) as connection:
                    self.assertIsNone(connection.execute(
                        "SELECT value FROM settings WHERE key='native_preference:kurisu.list.synced'"
                    ).fetchone())

    def test_legacy_schema_v1_is_left_for_core_to_upgrade(self):
        self.make_source(version=1)
        MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        with closing(sqlite3.connect(self.native / "kurisu.db")) as connection:
            self.assertEqual(connection.execute("PRAGMA user_version").fetchone(), (1,))
            self.assertEqual(connection.execute("SELECT progress FROM list_entry WHERE media_id=3").fetchone(), (2,))

    def test_every_supported_source_schema_version_is_preserved(self):
        for version in range(1, 9):
            with self.subTest(version=version):
                directory = self.root / f"schema-v{version}"
                directory.mkdir()
                self.legacy = directory / "legacy"
                self.legacy.mkdir()
                self.source = self.legacy / "kurisu.db"
                self.backup = directory / "backup.db"
                self.native = directory / "native-profile"
                self.make_source(version=version)
                if version == 8:
                    with closing(sqlite3.connect(self.source)) as connection:
                        connection.execute("UPDATE list_entry SET entry_id=NULL, queued_creation='fixture-creation'")
                        connection.execute("UPDATE pending_change SET payload=?", (json.dumps({
                            "media_id": 3, "base": None, "patch": {"creation_key": "fixture-creation"},
                            "conflict": False,
                        }),))
                        connection.commit()
                MIGRATE.transfer(self.source, self.export, self.backup, self.native)
                with closing(sqlite3.connect(self.native / "kurisu.db")) as connection:
                    self.assertEqual(connection.execute("PRAGMA user_version").fetchone(), (version,))
                    self.assertEqual(connection.execute("PRAGMA integrity_check").fetchone(), ("ok",))
                if version == 8:
                    for database in (self.backup, self.native / "kurisu.db"):
                        with closing(sqlite3.connect(database)) as connection:
                            self.assertEqual(connection.execute("SELECT queued_creation FROM list_entry").fetchone(), ("fixture-creation",))
                            self.assertEqual(connection.execute("SELECT json_extract(payload, '$.patch.creation_key') FROM pending_change").fetchone(), ("fixture-creation",))

    def test_existing_destination_and_backup_are_never_replaced(self):
        self.make_source()
        self.native.mkdir()
        sentinel = self.native / "keep"
        sentinel.write_text("existing")
        with self.assertRaisesRegex(MIGRATE.TransferError, "already exists"):
            MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertEqual(sentinel.read_text(), "existing")
        self.assertFalse(self.backup.exists())
        sentinel.unlink()
        self.native.rmdir()
        self.backup.write_bytes(b"existing")
        with self.assertRaisesRegex(MIGRATE.TransferError, "already exists"):
            MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertEqual(self.backup.read_bytes(), b"existing")
        self.assertFalse(self.native.exists())

    def test_existing_backup_sidecars_stop_before_publication(self):
        self.make_source()
        before = digest(self.source)
        for suffix in ("-wal", "-shm", "-journal"):
            with self.subTest(suffix=suffix):
                sidecar = Path(f"{self.backup}{suffix}")
                sidecar.write_bytes(b"preserve recovery data")
                try:
                    with self.assertRaises(MIGRATE.TransferError):
                        MIGRATE.transfer(self.source, self.export, self.backup, self.native)
                    self.assertFalse(self.backup.exists())
                    self.assertFalse(self.native.exists())
                    self.assertEqual(sidecar.read_bytes(), b"preserve recovery data")
                    self.assertEqual(digest(self.source), before)
                finally:
                    sidecar.unlink()

    def test_publication_failure_keeps_a_complete_backup_and_no_profile(self):
        self.make_source()
        original = digest(self.source)
        rename = MIGRATE._rename_no_replace

        def fail_profile_publication(stage, destination):
            if destination == self.native:
                raise MIGRATE.TransferError("synthetic publication failure")
            rename(stage, destination)

        with patch.object(MIGRATE, "_rename_no_replace", side_effect=fail_profile_publication):
            with self.assertRaisesRegex(MIGRATE.TransferError, "publication failure"):
                MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertFalse(self.native.exists())
        self.assertEqual(digest(self.source), original)
        with closing(sqlite3.connect(self.backup)) as connection:
            self.assertEqual(connection.execute("PRAGMA integrity_check").fetchone(), ("ok",))
            self.assertEqual(connection.execute("SELECT progress FROM list_entry WHERE media_id=3").fetchone(), (2,))
        self.assertEqual(list(self.root.glob(".kurisu-native-transfer-*")), [])
        backup_before = digest(self.backup)
        unavailable = self.root / "unavailable"
        self.legacy.rename(unavailable)
        try:
            with self.assertRaisesRegex(MIGRATE.TransferError, "File does not exist"):
                MIGRATE.transfer(self.source, self.export, self.backup, self.native)
            self.assertFalse(self.native.exists())
            self.assertEqual(digest(self.backup), backup_before)
        finally:
            unavailable.rename(self.legacy)
        MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertEqual(digest(self.backup), backup_before)
        self.assertTrue((self.native / MIGRATE.RECEIPT).is_file())

    def test_backup_publication_does_not_require_hard_links(self):
        self.make_source()
        with patch.object(MIGRATE.os, "link", side_effect=PermissionError("hard links unavailable")):
            MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        for database in (self.backup, self.native / "kurisu.db"):
            with closing(sqlite3.connect(database)) as connection:
                self.assertEqual(connection.execute("PRAGMA integrity_check").fetchone(), ("ok",))
                self.assertEqual(connection.execute("SELECT progress FROM list_entry").fetchone(), (2,))

    def test_backup_publication_refuses_a_late_collision(self):
        self.make_source()
        rename = MIGRATE._rename_no_replace

        def add_collision(stage, destination):
            if destination == self.backup:
                destination.write_bytes(b"preserve late collision")
            rename(stage, destination)

        with patch.object(MIGRATE, "_rename_no_replace", side_effect=add_collision):
            with self.assertRaisesRegex(MIGRATE.TransferError, "Backup destination appeared"):
                MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertEqual(self.backup.read_bytes(), b"preserve late collision")
        self.assertFalse(self.native.exists())

    def test_profile_publication_refuses_a_late_empty_directory(self):
        self.make_source()
        rename = MIGRATE._rename_no_replace
        identity = None

        def add_collision(stage, destination):
            nonlocal identity
            if destination == self.native:
                destination.mkdir()
                identity = destination.stat()
            rename(stage, destination)

        with patch.object(MIGRATE, "_rename_no_replace", side_effect=add_collision):
            with self.assertRaises(OSError):
                MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertIsNotNone(identity)
        self.assertTrue(os.path.samestat(self.native.stat(), identity))
        self.assertEqual(list(self.native.iterdir()), [])
        self.assertTrue(self.backup.is_file())

    def test_backup_sidecar_created_during_publication_blocks_profile(self):
        self.make_source()
        rename = MIGRATE._rename_no_replace
        sidecar = Path(f"{self.backup}-wal")

        def add_sidecar(stage, destination):
            if destination == self.backup:
                sidecar.write_bytes(b"preserve late sidecar")
            rename(stage, destination)

        with patch.object(MIGRATE, "_rename_no_replace", side_effect=add_sidecar):
            with self.assertRaisesRegex(MIGRATE.TransferError, "Backup path has SQLite sidecars"):
                MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertTrue(self.backup.is_file())
        self.assertEqual(sidecar.read_bytes(), b"preserve late sidecar")
        self.assertFalse(self.native.exists())

    def test_directory_sync_failure_reports_which_complete_copy_was_published(self):
        self.make_source()
        original = digest(self.source)
        sync = MIGRATE._sync_directory
        for phase in ("standalone backup", "native profile"):
            with self.subTest(phase=phase):
                backup_parent = self.root / f"{phase} backup parent"
                profile_parent = self.root / f"{phase} profile parent"
                backup_parent.mkdir()
                profile_parent.mkdir()
                backup = backup_parent / "backup.db"
                profile = profile_parent / "native"
                failed_parent = backup_parent if phase == "standalone backup" else profile_parent

                def fail_sync(path):
                    if path == failed_parent:
                        raise OSError(errno.ENOSPC, "synthetic directory sync failure")
                    sync(path)

                with patch.object(MIGRATE, "_sync_directory", side_effect=fail_sync):
                    for _ in range(2):
                        with self.assertRaisesRegex(MIGRATE.TransferError, f"published {phase}.*Errno 28"):
                            MIGRATE.transfer(self.source, self.export, backup, profile)
                self.assertEqual(profile.is_dir(), phase == "native profile")
                published = [backup, profile / "kurisu.db"] if profile.is_dir() else [backup]
                for database in published:
                    with closing(sqlite3.connect(database)) as connection:
                        self.assertEqual(connection.execute("PRAGMA integrity_check").fetchone(), ("ok",))
                        self.assertEqual(connection.execute("SELECT progress FROM list_entry").fetchone(), (2,))
                self.assertEqual(digest(self.source), original)
                before = {database: digest(database) for database in published}
                source = self.source.parent
                hidden = self.root / "unavailable"
                if profile.is_dir():
                    source.rename(hidden)
                try:
                    MIGRATE.transfer(self.source, self.export, backup, profile)
                finally:
                    if hidden.exists():
                        hidden.rename(source)
                self.assertEqual({database: digest(database) for database in published}, before)
                self.assertTrue((profile / MIGRATE.RECEIPT).is_file())
                with self.assertRaisesRegex(MIGRATE.TransferError, "belongs to another profile"):
                    MIGRATE.transfer(self.source, self.export, self.root / "other.db", profile)

    def test_source_change_at_stage_sync_refuses_publication(self):
        self.make_source()
        sync = MIGRATE._sync_directory

        def changed_source(path):
            sync(path)
            if path.name.startswith(".kurisu-native-transfer-"):
                with closing(sqlite3.connect(self.source)) as connection:
                    connection.execute("UPDATE list_entry SET progress=8 WHERE media_id=3")
                    connection.commit()

        with patch.object(MIGRATE, "_sync_directory", side_effect=changed_source):
            with self.assertRaisesRegex(MIGRATE.TransferError, "Source changed"):
                MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertFalse(self.native.exists())
        with closing(sqlite3.connect(self.backup)) as connection:
            self.assertEqual(connection.execute("SELECT progress FROM list_entry WHERE media_id=3").fetchone(), (2,))
        with closing(sqlite3.connect(self.source)) as connection:
            self.assertEqual(connection.execute("SELECT progress FROM list_entry WHERE media_id=3").fetchone(), (8,))

    def test_snapshot_rejects_a_transient_source_change_that_was_copied(self):
        self.make_source()
        original = self.source.read_bytes()
        copy = MIGRATE._copy_private

        def copy_changed_source(source, destination):
            if source != self.source:
                copy(source, destination)
                return
            with closing(sqlite3.connect(source)) as connection:
                connection.execute("UPDATE list_entry SET progress=8 WHERE media_id=3")
                connection.commit()
            try:
                copy(source, destination)
            finally:
                source.write_bytes(original)

        with patch.object(MIGRATE, "_copy_private", side_effect=copy_changed_source):
            with self.assertRaisesRegex(MIGRATE.TransferError, "Source changed during transfer"):
                MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertEqual(self.source.read_bytes(), original)
        self.assertFalse(self.backup.exists())
        self.assertFalse(self.native.exists())
        self.assertEqual(self.run_cli().returncode, 0)

    def test_native_copy_rejects_a_transient_backup_change(self):
        self.make_source()
        copy = MIGRATE._copy_private

        def copy_changed_backup(source, destination):
            if source != self.backup:
                copy(source, destination)
                return
            original = source.read_bytes()
            with closing(sqlite3.connect(source)) as connection:
                connection.execute("UPDATE list_entry SET progress=8 WHERE media_id=3")
                connection.commit()
            try:
                copy(source, destination)
            finally:
                source.write_bytes(original)

        with patch.object(MIGRATE, "_copy_private", side_effect=copy_changed_backup):
            with self.assertRaisesRegex(MIGRATE.TransferError, "Backup changed during transfer"):
                MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertFalse(self.native.exists())
        with closing(sqlite3.connect(self.backup)) as connection:
            self.assertEqual(connection.execute("SELECT progress FROM list_entry WHERE media_id=3").fetchone(), (2,))
        self.assertEqual(self.run_cli().returncode, 0)

    def test_retry_reports_that_the_rollback_backup_was_not_checked(self):
        self.make_source()
        first = self.run_cli()
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertIn(f"Standalone rollback backup: {self.backup}", first.stdout)
        self.backup.unlink()
        retry = self.run_cli()
        self.assertEqual(retry.returncode, 0, retry.stderr)
        self.assertIn("Native profile ready", retry.stdout)
        self.assertNotIn("Standalone rollback backup", retry.stdout)
        self.assertIn(f"Rollback backup was not checked on this retry: {self.backup}", retry.stderr)
        self.assertFalse(self.backup.exists())

    def test_retry_refuses_truncated_database_with_matching_receipt(self):
        self.make_source()
        self.assertEqual(self.run_cli().returncode, 0)
        database = self.native / "kurisu.db"
        database.write_bytes(database.read_bytes()[:101])
        before = {path.name: digest(path) for path in self.native.iterdir()}
        result = self.run_cli()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("Native database could not be validated", result.stderr)
        self.assertNotIn("Native profile ready", result.stdout)
        self.assertEqual(before, {path.name: digest(path) for path in self.native.iterdir()})

    def test_retry_validates_committed_native_wal_without_touching_profile(self):
        self.make_source()
        self.assertEqual(self.run_cli().returncode, 0)
        database = self.native / "kurisu.db"
        child = """
import os, sqlite3, sys
db = sqlite3.connect(sys.argv[1])
db.execute('PRAGMA journal_mode=WAL')
db.execute('UPDATE list_entry SET progress=5 WHERE media_id=3')
db.commit()
os._exit(0)
"""
        subprocess.run([sys.executable, "-c", child, str(database)], check=True)
        wal = database.with_name("kurisu.db-wal")
        shm = database.with_name("kurisu.db-shm")
        self.assertGreater(wal.stat().st_size, 32)
        self.legacy.rename(self.root / "unavailable")
        self.assertTrue(shm.is_file())
        for with_shm in (True, False):
            with self.subTest(with_shm=with_shm):
                if not with_shm:
                    shm.unlink()
                before = {path.name: digest(path) for path in self.native.iterdir()}
                result = self.run_cli()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("Native profile ready", result.stdout)
                self.assertEqual(before, {path.name: digest(path) for path in self.native.iterdir()})
        damaged = wal.read_bytes()[:-1]
        wal.write_bytes(damaged)
        result = self.run_cli()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("Native database could not be validated", result.stderr)
        self.assertFalse(shm.exists())
        self.assertEqual(wal.read_bytes(), damaged)

    def test_retry_detects_a_concurrent_commit_to_the_native_wal(self):
        self.make_source()
        MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.legacy.rename(self.root / "unavailable")
        validate = MIGRATE._validate_schema
        with closing(sqlite3.connect(self.native / "kurisu.db")) as writer:
            writer.execute("PRAGMA journal_mode=WAL")
            writer.execute("UPDATE list_entry SET progress=5 WHERE media_id=3")
            writer.commit()
            updated = {}

            def commit_during_validation(connection):
                validate(connection)
                writer.execute("UPDATE list_entry SET progress=6 WHERE media_id=3")
                writer.commit()
                updated.update({path.name: digest(path) for path in self.native.iterdir()})

            with patch.object(MIGRATE, "_validate_schema", side_effect=commit_during_validation):
                with self.assertRaisesRegex(MIGRATE.TransferError, "Native database changed during validation"):
                    MIGRATE.transfer(self.source, self.export, self.backup, self.native)
            result = self.run_cli()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(updated, {path.name: digest(path) for path in self.native.iterdir()})
            self.assertEqual(writer.execute("SELECT progress FROM list_entry").fetchone(), (6,))

    def test_preferences_change_during_native_population_retries_with_same_backup(self):
        self.make_source()
        populate = MIGRATE._populate_native

        def changed_preferences(backup, backup_digest, stage, values):
            populate(backup, backup_digest, stage, values)
            self.values["kurisu.list.q"] = "Updated query"
            self.write_export()

        with patch.object(MIGRATE, "_populate_native", side_effect=changed_preferences):
            with self.assertRaisesRegex(MIGRATE.TransferError, "Preference export changed"):
                MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertFalse(self.native.exists())
        before = digest(self.backup)
        MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertEqual(digest(self.backup), before)
        with closing(sqlite3.connect(self.native / "kurisu.db")) as connection:
            self.assertEqual(connection.execute(
                "SELECT value FROM settings WHERE key='native_preference:kurisu.list.q'"
            ).fetchone(), ("Updated query",))

    def test_invalid_export_and_schema_fail_without_publication(self):
        self.make_source()
        self.values["unexpected.secret"] = "synthetic"
        self.write_export()
        with self.assertRaisesRegex(MIGRATE.TransferError, "nine UI keys"):
            MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertFalse(self.backup.exists())
        self.assertFalse(self.native.exists())
        del self.values["unexpected.secret"]
        self.write_export()
        with closing(sqlite3.connect(self.source)) as connection:
            connection.execute("PRAGMA user_version=9")
        with self.assertRaisesRegex(MIGRATE.TransferError, "Unsupported legacy schema"):
            MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertFalse(self.backup.exists())
        self.assertFalse(self.native.exists())
        with closing(sqlite3.connect(self.source)) as connection:
            connection.execute("PRAGMA user_version=7")
            connection.execute("DROP TABLE pending_change")
        with self.assertRaisesRegex(MIGRATE.TransferError, "missing required tables"):
            MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertFalse(self.backup.exists())
        self.assertFalse(self.native.exists())

    def test_corrupt_database_and_rollback_journal_fail_closed(self):
        self.source.write_bytes(b"not sqlite")
        with self.assertRaises(MIGRATE.TransferError):
            MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertFalse(self.backup.exists())
        self.assertFalse(self.native.exists())
        self.source.unlink()
        self.make_source()
        Path(f"{self.source}-journal").write_bytes(b"synthetic journal")
        with self.assertRaisesRegex(MIGRATE.TransferError, "rollback journal"):
            MIGRATE.transfer(self.source, self.export, self.backup, self.native)
        self.assertFalse(self.backup.exists())
        self.assertFalse(self.native.exists())

    @unittest.skipIf(os.name == "nt", "Creating test symlinks requires Windows privilege")
    def test_symlink_paths_are_rejected_before_copy(self):
        self.make_source()
        linked = self.root / "linked"
        linked.symlink_to(self.legacy, target_is_directory=True)
        with self.assertRaisesRegex(MIGRATE.TransferError, "Links and reparse points"):
            MIGRATE.transfer(linked / "kurisu.db", self.export, self.backup, self.native)
        self.assertFalse(self.backup.exists())
        self.assertFalse(self.native.exists())

    def test_forbidden_component_is_rejected_before_filesystem_access(self):
        blocked = self.root / ("he" + "ntai") / "kurisu.db"
        with self.assertRaisesRegex(MIGRATE.TransferError, "permitted"):
            MIGRATE._check_path(blocked)

    def test_windows_reparse_attribute_is_rejected(self):
        junction = SimpleNamespace(st_mode=stat.S_IFDIR, st_file_attributes=0x400)
        self.assertTrue(MIGRATE._link_or_reparse(junction))
        ordinary = SimpleNamespace(st_mode=stat.S_IFDIR, st_file_attributes=0)
        self.assertFalse(MIGRATE._link_or_reparse(ordinary))


if __name__ == "__main__":
    unittest.main()
