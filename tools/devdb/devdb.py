#!/usr/bin/env python3
"""Edit uplink's database on a connected phone, for testing.

    devdb.py seed 20            twenty random contacts, some favourites, some called lately
    devdb.py add <key> [name]   a key saved as a contact — the one the CLI prints, say, which
                                has no code to scan

Works over adb on a debug build (`run-as`), with the standard library only. The app is stopped
first: the database is pulled whole, edited here and pushed back, and a running app would write
over it.
"""

import argparse
import random
import re
import secrets
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

DATABASE = "uplink.db"
# SQLite's write-ahead log and its index. The app runs in WAL mode, so recent writes can live here
# rather than in the database file itself.
SIDECARS = ("uplink.db-wal", "uplink.db-shm")
FILES = "files"
DEVICE_STAGING = "/data/local/tmp/uplink-devdb.db"
# How the app writes a key: iroh's `PublicKey` displays as 64 lowercase hex digits.
KEY = re.compile(r"^[0-9a-f]{64}$")

# Ed25519, for keys the app will accept: a public key is a curve point's y coordinate, with the
# sign of x in the top bit, and 32 random bytes are a point only about half the time.
FIELD = 2**255 - 19
CURVE_D = -121665 * pow(121666, -1, FIELD) % FIELD
SIGN_BIT = 255
KEY_BYTES = 32

NAMES = [
    "Aarav", "Aisha", "Amara", "Ananya", "Arjun", "Bilal", "Camille", "Chen Wei", "Dara", "Diego",
    "Elif", "Emeka", "Farah", "Hana", "Ibrahim", "Ines", "Jonas", "Kavya", "Kenji", "Layla",
    "Lucia", "Mariam", "Mateo", "Meera", "Nadia", "Noor", "Omar", "Priya", "Rahul", "Rania",
    "Rohan", "Sana", "Sofia", "Tariq", "Thandiwe", "Vikram", "Yara", "Yusuf", "Zainab", "Zoe",
]
# Shares of seeded contacts, so the list shows both of its groups and both kinds of second line.
FAVOURITE_SHARE = 0.15
CALLED_SHARE = 0.6
CALLED_WITHIN_DAYS = 45
SECONDS_PER_DAY = 86_400


def random_key() -> str:
    while True:
        y = secrets.randbelow(FIELD)
        y2 = y * y % FIELD
        x2 = (y2 - 1) * pow(CURVE_D * y2 + 1, -1, FIELD) % FIELD
        # A point exists for this y when x² is zero or a quadratic residue (Euler's criterion).
        if x2 == 0:
            return y.to_bytes(KEY_BYTES, "little").hex()
        if pow(x2, (FIELD - 1) // 2, FIELD) == 1:
            sign = secrets.randbelow(2)
            return (y | sign << SIGN_BIT).to_bytes(KEY_BYTES, "little").hex()


class Device:
    def __init__(self, package: str, user: str):
        self.package, self.user = package, user

    def shell(self, command: str, capture: bool = False) -> bytes:
        done = subprocess.run(["adb", "exec-out", command], check=True, capture_output=capture)
        return done.stdout

    def run_as(self, command: str, capture: bool = False) -> bytes:
        return self.shell(f"run-as {self.package} --user {self.user} {command}", capture)

    def stop(self):
        self.shell(f"am force-stop --user {self.user} {self.package}")

    def pull(self, into: Path):
        present = self.run_as(f"ls {FILES}", capture=True).decode().split()
        if DATABASE not in present:
            sys.exit(f"no {DATABASE} on the device yet: open uplink once first")
        for name in (DATABASE, *SIDECARS):
            if name in present:
                (into / name).write_bytes(self.run_as(f"cat {FILES}/{name}", capture=True))

    def push(self, database: Path):
        subprocess.run(["adb", "push", "-q", str(database), DEVICE_STAGING], check=True)
        sidecars = " ".join(f"{FILES}/{name}" for name in SIDECARS)
        self.run_as(f"sh -c 'cp {DEVICE_STAGING} {FILES}/{DATABASE} && rm -f {sidecars}'")
        self.shell(f"rm -f {DEVICE_STAGING}")

    def launch(self, activity: str):
        self.shell(f"am start --user {self.user} -n {self.package}/{activity}")


def contacts(db: sqlite3.Connection) -> dict[str, str]:
    """Saved keys and their names."""
    table = db.execute("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'contacts'").fetchone()
    if table is None:
        sys.exit("no contacts table yet: open uplink once first")
    return dict(db.execute("SELECT id, name FROM contacts"))


def free_name(wanted: str, taken: set[str]) -> str:
    """Names are unique in the table; a clash gets a number rather than failing."""
    name, n = wanted, 2
    while name in taken:
        name, n = f"{wanted} {n}", n + 1
    return name


def seed(db: sqlite3.Connection, count: int):
    taken = set(contacts(db).values())
    now = int(time.time())
    for _ in range(count):
        name = free_name(random.choice(NAMES), taken)
        taken.add(name)
        favourite = random.random() < FAVOURITE_SHARE
        called = (
            now - random.randrange(CALLED_WITHIN_DAYS * SECONDS_PER_DAY) if random.random() < CALLED_SHARE else None
        )
        db.execute(
            "INSERT INTO contacts (id, name, favourite, last_called) VALUES (?, ?, ?, ?)",
            (random_key(), name, int(favourite), called),
        )
    print(f"added {count} contacts")


def add(db: sqlite3.Connection, key: str, name: str):
    key = key.strip().lower()
    if not KEY.match(key):
        sys.exit(f"{key!r} is not a key: expected the 64 hex digits the CLI prints")
    saved = contacts(db)
    if key in saved:
        sys.exit(f"already saved as {saved[key]}")
    name = free_name(name, set(saved.values()))
    db.execute("INSERT INTO contacts (id, name) VALUES (?, ?)", (key, name))
    print(f"saved {key[:8]}… as {name}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--package", required=True)
    parser.add_argument("--user", default="0", help="Android user the app is installed for")
    parser.add_argument("--launch", metavar="ACTIVITY", help="start this activity afterwards")
    commands = parser.add_subparsers(dest="command", required=True)
    seeding = commands.add_parser("seed", help="add random contacts")
    seeding.add_argument("count", type=int)
    adding = commands.add_parser("add", help="save a key as a contact")
    adding.add_argument("key")
    adding.add_argument("name", nargs="?", default="CLI")
    args = parser.parse_args()

    device = Device(args.package, args.user)
    device.stop()
    with tempfile.TemporaryDirectory() as work:
        work = Path(work)
        device.pull(work)
        db = sqlite3.connect(work / DATABASE)
        with db:
            if args.command == "seed":
                seed(db, args.count)
            else:
                add(db, args.key, args.name)
        # Everything into the one file: the app reopens it in WAL mode by itself.
        db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        db.execute("PRAGMA journal_mode = DELETE")
        db.close()
        device.push(work / DATABASE)
    if args.launch:
        device.launch(args.launch)


if __name__ == "__main__":
    main()
