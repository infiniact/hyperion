#!/usr/bin/env python3
"""Generate a skyblock world: empty void everywhere except one starter island
in chunk (0,0). Hyperion serves missing region files as void, so we only write
r.0.0.mca; the rest of the world is air.

Island (chunk 0,0, blocks x,z in 0..15): a 16x16 dirt+grass platform (top grass
at Y63 → players stand at Y64, and spawn reliably lands on it), one oak tree, a
crafting table, and a single cobblestone "generator" block that the skyblock
plugin regenerates when mined.
"""
import struct, zlib, os, glob

END, BYTE, INT, LONG, STRING, LIST, COMPOUND, LONGARRAY = 0, 1, 3, 4, 8, 9, 10, 12

def _n(s):
    b = s.encode(); return struct.pack(">H", len(b)) + b
def tag(tid, nm, payload):
    return bytes([tid]) + _n(nm) + payload
def p_byte(v):  return struct.pack(">b", v)
def p_str(s):
    b = s.encode(); return struct.pack(">H", len(b)) + b
def p_long_array(vals):
    out = struct.pack(">i", len(vals))
    for v in vals:
        if v >= 2**63: v -= 2**64
        out += struct.pack(">q", v)
    return out
def p_list(tid, payloads):
    return bytes([tid]) + struct.pack(">i", len(payloads)) + b"".join(payloads)
def p_comp(tags):
    return b"".join(tags) + b"\x00"

def palette_entry(spec):
    if "[" in spec:
        nm, props = spec[:-1].split("[", 1)
        ptags = [tag(STRING, k, p_str(v)) for k, v in (kv.split("=") for kv in props.split(","))]
        return p_comp([tag(STRING, "Name", p_str(nm)), tag(COMPOUND, "Properties", p_comp(ptags))])
    return p_comp([tag(STRING, "Name", p_str(spec))])

AIR = "minecraft:air"
OVERRIDES = {}

def setb(x, y, z, spec):
    OVERRIDES[(x, y, z)] = spec

def block_at(x, y, z):
    return OVERRIDES.get((x, y, z)) or AIR

def build_island():
    # 16x16 platform spanning chunk (0,0)
    for x in range(16):
        for z in range(16):
            setb(x, 62, z, "minecraft:dirt")
            setb(x, 63, z, "minecraft:grass_block")
    # oak tree at (3,3)
    for y in range(64, 68):
        setb(3, y, 3, "minecraft:oak_log[axis=y]")
    for y in (66, 67):
        for dx in range(-2, 3):
            for dz in range(-2, 3):
                if abs(dx) == 2 and abs(dz) == 2:
                    continue
                if dx == 0 and dz == 0 and y == 66:
                    continue
                setb(3 + dx, y, 3 + dz, "minecraft:oak_leaves")
    for dx in range(-1, 2):
        for dz in range(-1, 2):
            setb(3 + dx, 68, 3 + dz, "minecraft:oak_leaves")
    setb(3, 69, 3, "minecraft:oak_leaves")
    # starter chest (right-click to claim a bonus kit) + cobblestone generator
    setb(8, 64, 8, "minecraft:chest")
    setb(12, 64, 4, "minecraft:cobblestone")

def build_section_nbt(cx, cz, sec_y):
    bx, bz = cx * 16, cz * 16
    pal, pal_idx, idxs = [], {}, []
    for i in range(4096):
        ly = i // 256; lz = (i // 16) % 16; lx = i % 16
        spec = block_at(bx + lx, sec_y * 16 + ly, bz + lz)
        j = pal_idx.get(spec)
        if j is None:
            j = len(pal); pal_idx[spec] = j; pal.append(spec)
        idxs.append(j)
    bs_tags = [tag(LIST, "palette", p_list(COMPOUND, [palette_entry(s) for s in pal]))]
    if len(pal) > 1:
        bits = max(4, (len(pal) - 1).bit_length()); per = 64 // bits
        longs = []
        for off in range(0, 4096, per):
            v = 0
            for k, idx in enumerate(idxs[off:off + per]):
                v |= idx << (k * bits)
            longs.append(v)
        bs_tags.append(tag(LONGARRAY, "data", p_long_array(longs)))
    bio = p_comp([tag(LIST, "palette", p_list(STRING, [p_str("minecraft:plains")]))])
    return p_comp([tag(BYTE, "Y", p_byte(sec_y)),
                   tag(COMPOUND, "block_states", p_comp(bs_tags)),
                   tag(COMPOUND, "biomes", bio)])

def build_chunk_bytes(cx, cz):
    sections = [build_section_nbt(cx, cz, sy) for sy in range(-4, 20)]
    root = tag(COMPOUND, "", p_comp([
        tag(LIST, "sections", p_list(COMPOUND, sections)),
        tag(LIST, "block_entities", p_list(END, [])),
    ]))
    return zlib.compress(root, 9)

def write_region(path, custom):
    SECTOR = 4096
    plain = build_chunk_bytes(99999, 99999)  # all-air chunk
    locations = bytearray(4096); timestamps = bytearray(4096); body = bytearray()
    cur = 2
    for slot in range(1024):
        comp = custom.get(slot, plain)
        rec = struct.pack(">I", len(comp) + 1) + b"\x02" + comp
        rec += b"\x00" * ((-len(rec)) % SECTOR)
        n = len(rec) // SECTOR
        struct.pack_into(">I", locations, slot * 4, (cur << 8) | (n & 0xFF))
        struct.pack_into(">I", timestamps, slot * 4, 1)
        body += rec; cur += n
    with open(path, "wb") as f:
        f.write(locations); f.write(timestamps); f.write(body)

def main():
    build_island()
    out = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", "map", "region"))
    os.makedirs(out, exist_ok=True)
    # clear any prior world (village etc.) so negative chunks become void
    for f in glob.glob(os.path.join(out, "r.*.mca")):
        os.remove(f)
    # island lives in chunk (0,0) → slot 0 of region r.0.0
    write_region(os.path.join(out, "r.0.0.mca"), {0: build_chunk_bytes(0, 0)})
    print(f"wrote skyblock map to {out} (16x16 island in chunk 0,0; rest is void)")

if __name__ == "__main__":
    main()
