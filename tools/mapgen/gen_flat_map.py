#!/usr/bin/env python3
"""Generate a superflat Minecraft world readable by Hyperion's chunk parser.

Output: ./map/region/r.{x}.{z}.mca  (a `region/` folder Hyperion can load via
HYPERION_MAP_DIR). Surface is grass at Y63 (players stand at Y64, within the
spawn scan range Y3..100 used by events/bedwars/src/plugin/spawn.rs).

Layers (per column): Y60 bedrock, Y61-62 dirt, Y63 grass_block, rest air.

Notes on the format Hyperion expects (crates/hyperion/.../loader/parse.rs):
  - chunk root compound holds `sections` (list) + `block_entities` (list)
  - each section: Y (byte), block_states{palette[, data]}, biomes{palette[, data]}
  - a single-entry palette needs NO data array (uniform section)
  - Hyperion ignores xPos/zPos, so every chunk can share identical NBT bytes
"""
import struct, zlib, os

# ---- minimal NBT writer ----
END, BYTE, INT, LONG, STRING, LIST, COMPOUND, LONGARRAY = 0, 1, 3, 4, 8, 9, 10, 12

def _name(s):
    b = s.encode(); return struct.pack(">H", len(b)) + b
def tag(tid, nm, payload):
    return bytes([tid]) + _name(nm) + payload
def p_byte(v):
    return struct.pack(">b", v)
def p_string(s):
    b = s.encode(); return struct.pack(">H", len(b)) + b
def p_long_array(vals):
    out = struct.pack(">i", len(vals))
    for v in vals:
        if v >= 2**63: v -= 2**64
        out += struct.pack(">q", v)
    return out
def p_list(elem_tid, payloads):
    return bytes([elem_tid]) + struct.pack(">i", len(payloads)) + b"".join(payloads)
def p_compound(tags):
    return b"".join(tags) + b"\x00"

def palette_entry(blockname):
    return p_compound([tag(STRING, "Name", p_string(blockname))])

def air_section(sec_y):
    bs = p_compound([tag(LIST, "palette", p_list(COMPOUND, [palette_entry("minecraft:air")]))])
    bio = p_compound([tag(LIST, "palette", p_list(STRING, [p_string("minecraft:plains")]))])
    return p_compound([
        tag(BYTE, "Y", p_byte(sec_y)),
        tag(COMPOUND, "block_states", bs),
        tag(COMPOUND, "biomes", bio),
    ])

def ground_section(sec_y):
    # sec_y=3 covers world Y 48..63; local y = Y-48
    pal = ["minecraft:air", "minecraft:bedrock", "minecraft:dirt", "minecraft:grass_block"]
    idxs = [0] * 4096
    for i in range(4096):
        y = i // 256  # i = y*256 + z*16 + x
        if y == 12:        idxs[i] = 1  # Y60 bedrock
        elif y in (13, 14): idxs[i] = 2  # Y61-62 dirt
        elif y == 15:      idxs[i] = 3  # Y63 grass
    # pack 4 bits/index, 16 per long (no cross-long spanning), LSB-first
    longs = []
    for l in range(256):
        v = 0
        for j in range(16):
            v |= (idxs[l * 16 + j] & 0xF) << (4 * j)
        longs.append(v)
    bs = p_compound([
        tag(LIST, "palette", p_list(COMPOUND, [palette_entry(n) for n in pal])),
        tag(LONGARRAY, "data", p_long_array(longs)),
    ])
    bio = p_compound([tag(LIST, "palette", p_list(STRING, [p_string("minecraft:plains")]))])
    return p_compound([
        tag(BYTE, "Y", p_byte(sec_y)),
        tag(COMPOUND, "block_states", bs),
        tag(COMPOUND, "biomes", bio),
    ])

def build_chunk():
    sections = [ground_section(y) if y == 3 else air_section(y) for y in range(-4, 20)]
    return tag(COMPOUND, "", p_compound([
        tag(LIST, "sections", p_list(COMPOUND, sections)),
        tag(LIST, "block_entities", p_list(END, [])),
    ]))

def write_region(path, comp):
    SECTOR = 4096
    record = struct.pack(">I", len(comp) + 1) + b"\x02" + comp  # length + zlib type + data
    record += b"\x00" * ((-len(record)) % SECTOR)
    sectors_per = len(record) // SECTOR
    locations = bytearray(4096)
    timestamps = bytearray(4096)
    body = bytearray()
    cur = 2  # header is 2 sectors
    for slot in range(1024):
        struct.pack_into(">I", locations, slot * 4, (cur << 8) | (sectors_per & 0xFF))
        struct.pack_into(">I", timestamps, slot * 4, 1)
        body += record
        cur += sectors_per
    with open(path, "wb") as f:
        f.write(locations); f.write(timestamps); f.write(body)

def main():
    out = os.path.join(os.path.dirname(__file__), "..", "..", "map", "region")
    out = os.path.abspath(out)
    os.makedirs(out, exist_ok=True)
    comp = zlib.compress(build_chunk(), 9)
    # regions (-1,-1),(-1,0),(0,-1),(0,0) -> blocks X[-512,511] Z[-512,511]
    for rx in (-1, 0):
        for rz in (-1, 0):
            write_region(os.path.join(out, f"r.{rx}.{rz}.mca"), comp)
    print(f"wrote 4 region files to {out} (flat grass at Y63, blocks X[-512,511] Z[-512,511])")

if __name__ == "__main__":
    main()
