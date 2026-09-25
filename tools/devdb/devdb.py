#!/usr/bin/env python3
"""Edit uplink's database on a connected phone, for testing.

    devdb.py seed contacts 20   twenty random contacts, some favourites, some called lately
    devdb.py seed calls 200     two hundred calls in the log, mostly with saved contacts, some with
                                keys nobody saved, over the last two months
    devdb.py add <key> [name]   a key saved as a contact — the one the CLI prints, say, which
                                has no code to scan
    devdb.py sql "<statement>"  one statement, for a hand migration: prints any rows it returns
                                and how many it changed

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
# Seeded calls: how far back, how many are with keys nobody saved, and which way each outcome can
# go (as `calls::Outcome` stores it), weighted roughly as a phone's log reads.
CALLS_WITHIN_DAYS = 60
STRANGER_SHARE = 0.1
VOICE_SHARE = 0.4
OUTCOMES = [
    # (outcome, incoming: True / False / None for either, weight)
    ("answered", None, 50),
    ("missed", True, 14),
    ("declined", True, 4),
    ("screened", True, 3),
    ("rejected", False, 4),
    ("cancelled", False, 8),
    ("no-answer", False, 8),
    ("unreachable", False, 4),
    ("lost", None, 3),
    ("failed", None, 2),
]
ANSWERED = ("answered", "lost")
LONGEST_CALL_SECONDS = 3_600
# Bytes a second each way for an answered call's traffic: a voice call, a video one.
VOICE_BYTES_PER_SECOND = 6_000
VIDEO_BYTES_PER_SECOND = 450_000


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
        listing = subprocess.run(
            ["adb", "exec-out", f"run-as {self.package} --user {self.user} ls {FILES}"], capture_output=True
        )
        said = (listing.stdout + listing.stderr).decode()
        # run-as says so on stdout and still exits 0 through exec-out.
        if "not debuggable" in said:
            sys.exit(f"{self.package} is a release build: install a debug one (`just apk && just install`)")
        present = said.split()
        if DATABASE not in present:
            sys.exit(f"no {DATABASE} on the device yet: open uplink once first")
        for name in (DATABASE, *SIDECARS):
            if name in present:
                (into / name).write_bytes(self.run_as(f"cat {FILES}/{name}", capture=True))

    def push(self, database: Path):
        # Quiet by discarding its line: `push -q` is newer than some adb builds (Debian's 34).
        subprocess.run(["adb", "push", str(database), DEVICE_STAGING], check=True, stdout=subprocess.DEVNULL)
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


def seed_calls(db: sqlite3.Connection, count: int):
    saved = list(contacts(db))
    if db.execute("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'calls'").fetchone() is None:
        sys.exit("no calls table yet: open uplink once first")
    now = int(time.time())
    last_called: dict[str, int] = {}
    weights = [weight for _, _, weight in OUTCOMES]
    for _ in range(count):
        peer = random_key() if not saved or random.random() < STRANGER_SHARE else random.choice(saved)
        outcome, way, _ = random.choices(OUTCOMES, weights)[0]
        incoming = random.random() < 0.5 if way is None else way
        at = now - random.randrange(CALLS_WITHIN_DAYS * SECONDS_PER_DAY)
        voice = random.random() < VOICE_SHARE
        seconds = sent = received = None
        if outcome in ANSWERED:
            seconds = random.randrange(1, LONGEST_CALL_SECONDS)
            rate = VOICE_BYTES_PER_SECOND if voice else VIDEO_BYTES_PER_SECOND
            sent, received = (int(seconds * rate * random.uniform(0.6, 1.2)) for _ in range(2))
        db.execute(
            "INSERT INTO calls (peer, incoming, outcome, at, seconds, sent, received, voice) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            (peer, int(incoming), outcome, at, seconds, sent, received, int(voice)),
        )
        if peer in saved:
            last_called[peer] = max(at, last_called.get(peer, 0))
    for peer, at in last_called.items():
        db.execute("UPDATE contacts SET last_called = max(coalesce(last_called, 0), ?) WHERE id = ?", (at, peer))
    print(f"added {count} calls")


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


def run_sql(db: sqlite3.Connection, statement: str):
    for row in db.execute(statement).fetchall():
        print(row)
    print(f"{db.total_changes} rows changed")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--package", required=True)
    parser.add_argument("--user", default="0", help="Android user the app is installed for")
    parser.add_argument("--launch", metavar="ACTIVITY", help="start this activity afterwards")
    commands = parser.add_subparsers(dest="command", required=True)
    seeding = commands.add_parser("seed", help="add random contacts or calls")
    seeding.add_argument("kind", choices=["contacts", "calls"])
    seeding.add_argument("count", type=int)
    adding = commands.add_parser("add", help="save a key as a contact")
    adding.add_argument("key")
    adding.add_argument("name", nargs="?", default="CLI")
    running = commands.add_parser("sql", help="run one statement")
    running.add_argument("statement")
    args = parser.parse_args()

    device = Device(args.package, args.user)
    device.stop()
    with tempfile.TemporaryDirectory() as work:
        work = Path(work)
        device.pull(work)
        db = sqlite3.connect(work / DATABASE)
        with db:
            if args.command == "seed" and args.kind == "contacts":
                seed(db, args.count)
            elif args.command == "seed":
                seed_calls(db, args.count)
            elif args.command == "sql":
                run_sql(db, args.statement)
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
