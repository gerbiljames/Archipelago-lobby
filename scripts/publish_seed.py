#!/usr/bin/env python3
"""Publish a locally-generated AP seed to a lobby room.

Default mode uploads AP_<seed>.zip together with the AP_<seed>_slot_passwords.json
written next to it (if any); the lobby checks the seed against the room's slots,
associates patches, and sets per-slot passwords.

    LOBBY_ADMIN_TOKEN=... ./publish_seed.py --room-id <uuid> --seed AP_<seed>.zip

--passwords-only sets just the passwords (matched by slot number) via the
per-yaml API, without touching patches.
"""

import argparse
import json
import mimetypes
import os
import sys
import urllib.error
import urllib.request
import uuid
import zipfile

TIMEOUT = 300


def sidecar_passwords_path(seed_path):
    base, _ = os.path.splitext(seed_path)
    return f"{base}_slot_passwords.json"


def resolve_passwords_path(args):
    if args.passwords:
        if not os.path.exists(args.passwords):
            sys.exit(f"Passwords file not found: {args.passwords}")
        return args.passwords
    sidecar = sidecar_passwords_path(args.seed)
    return sidecar if os.path.exists(sidecar) else None


def load_passwords(args):
    path = resolve_passwords_path(args)
    if path:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    # Older generators wrote the export inside the zip.
    with zipfile.ZipFile(args.seed) as zf:
        names = [n for n in zf.namelist() if n.endswith("_slot_passwords.json")]
        if len(names) == 1:
            return json.loads(zf.read(names[0]))
    sys.exit(f"No slot passwords found: expected {sidecar_passwords_path(args.seed)} next to the "
             "zip (was per_slot_passwords enabled in host.yaml?) or pass --passwords")


def encode_multipart(fields):
    boundary = uuid.uuid4().hex
    body = bytearray()
    for name, (filename, content) in fields.items():
        content_type = mimetypes.guess_type(filename)[0] or "application/octet-stream"
        body += (f"--{boundary}\r\n"
                 f'Content-Disposition: form-data; name="{name}"; filename="{filename}"\r\n'
                 f"Content-Type: {content_type}\r\n\r\n").encode()
        body += content
        body += b"\r\n"
    body += f"--{boundary}--\r\n".encode()
    return bytes(body), f"multipart/form-data; boundary={boundary}"


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
        fields = {"seed": (os.path.basename(args.seed), f.read())}
    passwords_path = resolve_passwords_path(args)
    if passwords_path:
        with open(passwords_path, "rb") as f:
            fields["passwords"] = (os.path.basename(passwords_path), f.read())
    else:
        print("No slot passwords file found next to the seed; existing passwords are kept "
              "unless the zip bundles one")

    body, content_type = encode_multipart(fields)
    result = api(args.base_url, args.token, "POST",
                 f"/api/room/{args.room_id}/upload_generation",
                 body=body, content_type=content_type)
    print(f"Uploaded {os.path.basename(args.seed)} ({len(fields['seed'][1])} bytes)")
    print(f"  seed {result['seed_name']}, generation {result['job_id']}")
    print(f"  {result['patches_associated']} patches associated, "
          f"{result['passwords_set']} passwords set")
    for warning in result.get("warnings", []):
        print(f"  ~ {warning}")


def passwords_only(args):
    slots = load_passwords(args)
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
    parser.add_argument("--passwords",
                        help="slot passwords JSON (defaults to AP_<seed>_slot_passwords.json next to the zip)")
    parser.add_argument("--base-url", default="https://ap-lobby.fightme.pro")
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
