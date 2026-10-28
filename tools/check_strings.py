#!/usr/bin/env python3
"""Verify Nozavolochen string tables (read-only).

Checks every loader/strings/<code>.json:
- valid JSON, flat string map, no empty values;
- every file has exactly the en.json key set (missing/extra fail);
- values fit the overlay panel (at most MAX_LEN chars);
- <code> is a game language the loader knows (see strings.rs SUPPORTED).

Usage: python3 tools/check_strings.py [strings dir]
"""

import json
import os
import sys

SUPPORTED = {
    "en", "de", "fr", "it", "es", "es-419", "ru", "pl", "nl",
    "pt", "pt-BR", "ja", "zh", "zh-Hant", "ko",
}
MAX_LEN = 48


def main():
    sdir = sys.argv[1] if len(sys.argv) > 1 else os.path.join("loader", "strings")
    en_path = os.path.join(sdir, "en.json")
    try:
        en = json.load(open(en_path, encoding="utf-8"))
    except (OSError, ValueError) as e:
        print(f"en.json: FAIL {e}")
        return 1
    assert isinstance(en, dict) and en, "en.json must be a non-empty object"
    failures = 0
    for key, val in sorted(en.items()):
        if not isinstance(val, str) or not val:
            print(f"en.json: FAIL {key!r} is not a non-empty string")
            failures += 1
        elif len(val) > MAX_LEN:
            print(f"en.json: FAIL {key!r} is {len(val)} chars, max {MAX_LEN}")
            failures += 1
    en_keys = set(en)
    for fname in sorted(os.listdir(sdir)):
        if not fname.endswith(".json") or fname == "en.json":
            continue
        code = fname[:-5]
        path = os.path.join(sdir, fname)
        if code not in SUPPORTED:
            print(f"{fname}: FAIL unknown language code {code!r}")
            failures += 1
            continue
        try:
            got = json.load(open(path, encoding="utf-8"))
        except ValueError as e:
            print(f"{fname}: FAIL {e}")
            failures += 1
            continue
        keys = set(got)
        missing = sorted(en_keys - keys)
        extra = sorted(keys - en_keys)
        if missing or extra:
            print(f"{fname}: FAIL missing {missing}, extra {extra}")
            failures += 1
            continue
        bad = [k for k, v in got.items()
               if not isinstance(v, str) or not v or len(v) > MAX_LEN]
        if bad:
            print(f"{fname}: FAIL bad values for {sorted(bad)}")
            failures += 1
            continue
        print(f"{fname}: OK {len(keys)} keys")
    print(f"check_strings: {'OK' if not failures else f'{failures} FAILURE(S)'}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
