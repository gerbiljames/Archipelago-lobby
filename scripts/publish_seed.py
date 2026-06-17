#!/usr/bin/env python3
"""Publish a locally-generated AP seed to a lobby room.

Default mode uploads the whole AP_<seed>.zip; the lobby ingests it, associates
patches, and sets per-slot passwords from the bundled slot_passwords.json.

    LOBBY_ADMIN_TOKEN=... ./publish_seed.py --room-id <uuid> --seed AP_<seed>.zip

--passwords-only sets just the passwords (matched by slot number) via the
per-yaml API, without touching patches.
"""

import argparse
import json
import os
import sys
import urllib.error
import urllib.request
import zipfile

TIMEOUT = 300


def load_passwords(seed_path):
    with zipfile.ZipFile(seed_path) as zf:
        names = [n for n in zf.namelist() if n.endswith("_slot_passwords.json")]
        if not names:
            sys.exit(f"No *_slot_passwords.json in {seed_path} "
                     "(was per_slot_passwords enabled in host.yaml?)")
        if len(names) > 1:
            sys.exit(f"Multiple slot-password exports in {seed_path}: {names}")
        return json.loads(zf.read(names[0]))


def api(base_url, token, method, path, body=None, content_type="application/json"):
    url = f"{base_url.rstrip('/')}{path}"
    if body is None:
        payload = None
    elif isinstance(body, (bytes, bytearray)):
        payload = body
    else:
        payload = json.dumps(body).encode()
    req = urllib.request.Request(url, data=payload, method=method)
    req.add_header("X-Api-Key", token)
    if payload is not None:
        req.add_header("Content-Type", content_type)
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            raw = resp.read()
            return json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        sys.exit(f"{method} {path} failed: {e.code} {e.reason}\n{e.read().decode(errors='replace')}")
    except urllib.error.URLError as e:
        sys.exit(f"{method} {path} failed: {e.reason}")


def upload(args):
    with open(args.seed, "rb") as f:
        zip_bytes = f.read()
    result = api(args.base_url, args.token, "POST",
                 f"/api/room/{args.room_id}/upload_generation",
                 body=zip_bytes, content_type="application/zip")
    print(f"Uploaded {os.path.basename(args.seed)} ({len(zip_bytes)} bytes)")
    print(f"  generation {result['job_id']}")
    print(f"  {result['patches_associated']} patches associated, "
          f"{result['passwords_set']} passwords set")


def passwords_only(args):
    slots = load_passwords(args.seed)
    room = api(args.base_url, args.token, "GET", f"/api/room/{args.room_id}")
    by_slot = {y["slot_number"]: y for y in room["yamls"]}

    print(f"Room {room['name']!r}: {len(room['yamls'])} slots; "
          f"AP export has {len(slots)} passwords\n")

    set_count = warnings = errors = 0
    for entry in sorted(slots, key=lambda e: e["slot"]):
        slot, ap_name, password = entry["slot"], entry["name"], entry["password"]
        yaml = by_slot.get(slot)
        if yaml is None:
            print(f"  ! slot {slot} ({ap_name!r}): no matching lobby slot — skipped")
            errors += 1
            continue
        if yaml["player_name"] != ap_name:
            print(f"  ~ slot {slot}: name differs — lobby {yaml['player_name']!r} "
                  f"vs AP {ap_name!r} (matched by slot number)")
            warnings += 1
        if args.dry_run:
            print(f"    [dry-run] slot {slot} {yaml['player_name']!r} -> {password}")
        else:
            api(args.base_url, args.token, "POST",
                f"/api/room/{args.room_id}/set_password/{yaml['id']}",
                {"password": password})
            print(f"    set slot {slot} {yaml['player_name']!r}")
        set_count += 1

    print(f"\n{'Would set' if args.dry_run else 'Set'} {set_count}; "
          f"{warnings} name warning(s), {errors} unmatched.")
    if errors:
        sys.exit(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--room-id", required=True)
    parser.add_argument("--seed", required=True, help="AP seed zip")
    parser.add_argument("--base-url", default="https://fightme.pro")
    parser.add_argument("--token", default=os.environ.get("LOBBY_ADMIN_TOKEN"),
                        help="admin token (defaults to $LOBBY_ADMIN_TOKEN)")
    parser.add_argument("--passwords-only", action="store_true",
                        help="set passwords only, don't upload patches")
    parser.add_argument("--dry-run", action="store_true",
                        help="passwords-only: show changes without applying")
    args = parser.parse_args()

    if not args.token:
        sys.exit("No admin token: pass --token or set LOBBY_ADMIN_TOKEN")

    if args.passwords_only:
        passwords_only(args)
    else:
        upload(args)


if __name__ == "__main__":
    main()
