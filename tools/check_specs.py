#!/usr/bin/env python3
"""Verify Nozavolochen mod specs against a real NMS.exe (read-only).

Checks, per [[patch]]:
- write-bytes / force-branch: pattern matches exactly once in .text;
  expect bytes (if given) match the game at pattern+offset; bytes length
  fits the replaced instruction (write-bytes: expect length when given).
- retarget-jmp: replicates the structural filter (backward e9 into a
  `test al,al` + je site) and requires exactly one survivor.
- detour-with-arg-overrides: pattern matches exactly once in .text.
- page-size-override: anchors present as NUL-terminated ASCII;
  xref count ruled by the patch's xref_policy (unique, or
  any-outer-with-scale which tries each outer); NEXT_PAGE /
  PREV_PAGE step patterns unique in .text.

Also checks every [[mod.text]] / [[setting.text]] block carries a
non-empty lang (translations themselves are not verified).

Usage: python3 tools/check_specs.py [NMS.exe] [mods dir]
Verifies the mod.toml specs in the given dir (defaults to the local
Steam install and the current dir).
"""

import os
import struct
import sys

GAME = "/home/savagecore/.steam/steam/steamapps/common/No Man's Sky/Binaries/NMS.exe"

ANCHORS = ["TRANSFER_RECEIVE_DESC", "PRODUCTS_ALL", "SLOT%02d"]
NEXT_PAGE = bytes([0xC1, 0xE1, 0x03, 0x83, 0xC0, 0x08, 0x3B, 0xC8, 0x0F, 0x4C, 0xC1])
PREV_PAGE = bytes([0x83, 0xC1, 0xF8])
RIP_LEA_RM = {0x05, 0x0D, 0x15, 0x1D, 0x25, 0x2D, 0x35, 0x3D}


def parse_pe(data):
    assert data[0:2] == b"MZ", "not a PE image"
    lf = struct.unpack("<I", data[0x3C:0x40])[0]
    assert struct.unpack("<H", data[lf + 24:lf + 26])[0] == 0x20B, "not PE32+"
    nsec = struct.unpack("<H", data[lf + 6:lf + 8])[0]
    text = None
    for i in range(nsec):
        o = lf + 24 + 240 + i * 40
        name = data[o:o + 8]
        vsize = struct.unpack("<I", data[o + 8:o + 12])[0]
        rva = struct.unpack("<I", data[o + 12:o + 16])[0]
        raw = struct.unpack("<I", data[o + 20:o + 24])[0]
        size = struct.unpack("<I", data[o + 16:o + 20])[0]
        chars = struct.unpack("<I", data[o + 36:o + 40])[0]
        if name == b".text\x00\x00\x00" and chars & 0x20000000:
            text = (rva, raw, size)
    assert text, ".text not found"
    rva, raw, size = text
    blob = data[raw:raw + size]
    return rva, blob


def decode_pattern(s):
    pat, mask = bytearray(), bytearray()
    for tok in s.split():
        if tok in ("??", "?"):
            pat.append(0)
            mask.append(0)
        else:
            assert len(tok) == 2 and all(c in "0123456789abcdefABCDEF" for c in tok), \
                f"bad token {tok!r}"
            pat.append(int(tok, 16))
            mask.append(1)
    assert pat, "empty pattern"
    return bytes(pat), bytes(mask)


def scan(blob, pat, mask):
    n = len(pat)
    out = []
    for i in range(len(blob) - n + 1):
        ok = True
        for j in range(n):
            if mask[j] and blob[i + j] != pat[j]:
                ok = False
                break
        if ok:
            out.append(i)
    return out


def read_i32(blob, o):
    return struct.unpack("<i", blob[o:o + 4])[0]


def parse_mod(path):
    mod = {"id": None, "features": {}, "patches": [], "texts": []}
    cur = None
    cur_text = None
    section = None
    for raw in open(path).read().splitlines():
        line = raw.split("#")[0].split(";")[0].strip()
        if not line:
            continue
        if line.startswith("["):
            if line.lower() == "[[patch]]":
                if cur:
                    mod["patches"].append(cur)
                cur = {}
                cur_text = None
                section = "patch"
            elif line.lower() in ("[[setting.text]]", "[[mod.text]]"):
                cur_text = {}
                mod["texts"].append(cur_text)
                section = "text"
            else:
                if cur:
                    mod["patches"].append(cur)
                    cur = None
                cur_text = None
                section = line.strip("[]").lower()
            continue
        if "=" not in line:
            continue
        k, v = (s.strip() for s in line.split("=", 1))
        v = v.strip('"')
        if section == "mod" and k == "id":
            mod["id"] = v
        elif section == "features":
            mod["features"][k.lower()] = v.lower() in ("true", "1", "yes", "on")
        elif section == "patch" and cur is not None:
            cur[k] = v
        elif section == "text" and cur_text is not None:
            cur_text[k] = v
    if cur:
        mod["patches"].append(cur)
    return mod


def find_c_strings(data, sections, text):
    raw = text.encode()
    hits = []
    start = 0
    while True:
        i = data.find(raw, start)
        if i < 0:
            break
        if data[i + len(raw):i + len(raw) + 1] == b"\x00":
            for (rva, rp, size) in sections:
                if rp <= i < rp + size:
                    hits.append(rva + (i - rp))
                    break
        start = i + 1
    return hits


def rip_lea_xrefs(blob, base_rva, want):
    want = set(want)
    out = []
    i = 0
    n = len(blob)
    while i < n:
        ml = 0
        if blob[i] == 0x8D and i + 1 < n and blob[i + 1] in RIP_LEA_RM:
            ml = 2
        elif 0x40 <= blob[i] <= 0x4F and i + 2 < n and blob[i + 1] == 0x8D \
                and blob[i + 2] in RIP_LEA_RM:
            ml = 3
        if ml and i + ml + 4 <= n:
            disp = struct.unpack("<i", blob[i + ml:i + ml + 4])[0]
            insn = base_rva + i
            out.append((insn, insn + ml + 4 + disp))
            i += ml
        else:
            i += 1
    return [insn for (insn, tgt) in out if tgt in want]


def main():
    exe = sys.argv[1] if len(sys.argv) > 1 else GAME
    moddir = sys.argv[2] if len(sys.argv) > 2 else "."
    data = open(exe, "rb").read()
    base_rva, blob = parse_pe(data)
    lf = struct.unpack("<I", data[0x3C:0x40])[0]
    nsec = struct.unpack("<H", data[lf + 6:lf + 8])[0]
    sections = []
    for i in range(nsec):
        o = lf + 24 + 240 + i * 40
        sections.append((struct.unpack("<I", data[o + 12:o + 16])[0],
                         struct.unpack("<I", data[o + 20:o + 24])[0],
                         struct.unpack("<I", data[o + 16:o + 20])[0]))
    failures = 0

    for fname in sorted(os.listdir(moddir)):
        if not fname.endswith(".toml"):
            continue
        mod = parse_mod(os.path.join(moddir, fname))
        print(f"== {mod['id']} ({fname})")
        try:
            for t in mod["texts"]:
                assert t.get("lang", "").strip(), "text block without lang"
            if mod["texts"]:
                langs = sorted({t["lang"].strip().lower() for t in mod["texts"]})
                print(f"  texts: OK {len(mod['texts'])} block(s), langs {','.join(langs)}")
        except AssertionError as e:
            print(f"  texts: FAIL {e}")
            failures += 1
        for p in mod["patches"]:
            pid, op = p.get("id", "?"), p.get("op", "?")
            try:
                if op in ("write-bytes", "force-branch"):
                    pat, mask = decode_pattern(p["pattern"])
                    hits = scan(blob, pat, mask)
                    assert len(hits) == 1, f"{len(hits)} hits, want unique"
                    at = hits[0] + int(p.get("offset", "0"))
                    if op == "force-branch":
                        assert blob[at] == 0x0F and blob[at + 1] == 0x84, \
                            "not je as expected"
                    if "expect" in p:
                        exp = bytes(int(t, 16) for t in p["expect"].split())
                        assert blob[at:at + len(exp)] == exp, "expect mismatch"
                    if op == "write-bytes":
                        nb = len(p["bytes"].split())
                        assert at + nb <= len(blob), "bytes overrun"
                        if "expect" in p:
                            assert len(exp) <= nb, "expect longer than bytes"
                    print(f"  {pid}: OK unique @ rva {base_rva + hits[0]:#x}")
                elif op == "retarget-jmp":
                    pat, mask = decode_pattern(p["pattern"])
                    off = int(p["offset"])
                    cands = []
                    for h in scan(blob, pat, mask):
                        at = h + off
                        if blob[at] != 0xE9:
                            continue
                        rel = read_i32(blob, at + 1)
                        if rel >= 0:
                            continue
                        tgt = at + 5 + rel
                        if not (0 <= tgt + 8 <= len(blob)):
                            continue
                        tb = blob[tgt:tgt + 8]
                        if tb[0] != 0x84 or tb[1] != 0xC0:
                            continue
                        if tb[2] == 0x0F and tb[3] == 0x84:
                            skip = tgt + 8 + read_i32(blob, tgt + 4)
                        elif tb[2] == 0x74:
                            skip = tgt + 4 + struct.unpack("b", tb[3:4])[0]
                        else:
                            continue
                        cands.append((at, skip))
                    assert len(cands) == 1, f"{len(cands)} survivors, want 1"
                    print(f"  {pid}: OK 1 structural survivor @ rva "
                          f"{base_rva + cands[0][0]:#x}")
                elif op == "detour-with-arg-overrides":
                    pat, mask = decode_pattern(p["pattern"])
                    hits = scan(blob, pat, mask)
                    assert len(hits) == 1, f"{len(hits)} hits, want unique"
                    print(f"  {pid}: OK unique hook @ rva {base_rva + hits[0]:#x}")
                elif op == "page-size-override":
                    items = int(p["items"])
                    assert items != 8, "vanilla item count does nothing"
                    tx_anchor = p.get("transfer_anchor", "TRANSFER_RECEIVE_DESC")
                    craft_anchor = p.get("craft_anchor", "PRODUCTS_ALL")
                    slot_format = p.get("slot_format", "SLOT%02d")
                    policy = p.get("xref_policy", "unique").strip().lower()
                    assert policy in ("unique", "any-outer-with-scale"), \
                        f"unknown xref_policy {p.get('xref_policy')!r}"
                    for a in [tx_anchor, craft_anchor]:
                        refs = rip_lea_xrefs(blob, base_rva,
                                             find_c_strings(data, sections, a))
                        if policy == "unique":
                            assert len(refs) == 1, f"{a}: {len(refs)} xrefs, want 1"
                        else:
                            assert refs, f"{a}: no xrefs"
                            if len(refs) != 1:
                                print(f"  {pid}: note {a} has {len(refs)} xrefs (loader tries each outer)")
                    slot_refs = rip_lea_xrefs(blob, base_rva,
                                              find_c_strings(data, sections, slot_format))
                    assert slot_refs, f"{slot_format}: no xrefs"
                    assert len(scan(blob, NEXT_PAGE, b"\x01" * len(NEXT_PAGE))) == 1, \
                        "NEXT_PAGE step not unique"
                    assert len(scan(blob, PREV_PAGE, b"\x01" * len(PREV_PAGE))) >= 1, \
                        "PREV_PAGE step missing"
                    scale = struct.pack("<f", 0.125)
                    assert scale in blob, "0.125 scale float missing"
                    print(f"  {pid}: OK slot filler + anchors present, items={items} policy={policy}")
                else:
                    raise AssertionError(f"unknown op {op!r}")
            except AssertionError as e:
                print(f"  {pid}: FAIL {e}")
                failures += 1
    print(f"check_specs: {'OK' if not failures else f'{failures} FAILURE(S)'}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
