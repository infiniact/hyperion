#!/usr/bin/env python3
"""Generate a flat world with a small village (houses + roads + farm + trees).

Pure block data baked into region files — Hyperion loads it via HYPERION_MAP_DIR,
no engine recompile needed. Output: ./map/region/r.{x}.{z}.mca

Base terrain (per column): Y60 bedrock, Y61-62 dirt, Y63 grass, rest air.
Everything else (roads at Y63, structures from Y64 up) is layered on top via the
`OVERRIDES` dict. Players spawn at (0,0) on the central crossroads.
"""
import struct, zlib, os

# ---- minimal NBT writer (same format Hyperion's parser reads) ----
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
    """spec: 'minecraft:cobblestone' or 'minecraft:wheat[age=7]'."""
    if "[" in spec:
        nm, props = spec[:-1].split("[", 1)
        ptags = []
        for kv in props.split(","):
            k, v = kv.split("=")
            ptags.append(tag(STRING, k, p_str(v)))
        return p_comp([tag(STRING, "Name", p_str(nm)),
                       tag(COMPOUND, "Properties", p_comp(ptags))])
    return p_comp([tag(STRING, "Name", p_str(spec))])

# ---- world model ----
AIR = "minecraft:air"
OVERRIDES = {}  # (x,y,z) -> block spec, anything different from base flat terrain

def base_block(y):
    if y == 60: return "minecraft:bedrock"
    if y in (61, 62): return "minecraft:dirt"
    if y == 63: return "minecraft:grass_block"
    return AIR

def block_at(x, y, z):
    return OVERRIDES.get((x, y, z)) or base_block(y)

def setb(x, y, z, spec):
    OVERRIDES[(x, y, z)] = spec

# track surface usage so trees/flowers don't grow on roads/houses/farm
OCCUPIED = set()
def occupy(x, z):
    OCCUPIED.add((x, z))

# ---- structures ----
def carve_roads():
    # plus-shaped cobblestone roads, 3 wide, replacing the grass surface (Y63)
    for x in range(-48, 49):
        for dz in (-1, 0, 1):
            setb(x, 63, dz, "minecraft:cobblestone"); occupy(x, dz)
    for z in range(-48, 49):
        for dx in (-1, 0, 1):
            setb(dx, 63, z, "minecraft:cobblestone"); occupy(dx, z)

def build_house(x0, z0, w=7, d=7, door_side="-z"):
    """A simple oak house. (x0,z0) = min corner of footprint."""
    x1, z1 = x0 + w - 1, z0 + d - 1
    floor_y, wall_top = 63, 67   # walls Y64..67
    for x in range(x0, x1 + 1):
        for z in range(z0, z1 + 1):
            setb(x, floor_y, z, "minecraft:oak_planks"); occupy(x, z)  # floor
    for y in range(64, wall_top + 1):
        for x in range(x0, x1 + 1):
            for z in range(z0, z1 + 1):
                edge = x in (x0, x1) or z in (z0, z1)
                if not edge:
                    continue
                corner = x in (x0, x1) and z in (z0, z1)
                if corner:
                    setb(x, y, z, "minecraft:oak_log[axis=y]")
                elif y in (65, 66) and (x == (x0 + x1) // 2 or z == (z0 + z1) // 2):
                    setb(x, y, z, "minecraft:glass")        # windows
                else:
                    setb(x, y, z, "minecraft:oak_planks")
    # roof
    for x in range(x0, x1 + 1):
        for z in range(z0, z1 + 1):
            setb(x, wall_top + 1, z, "minecraft:oak_planks")
    # ceiling light
    setb((x0 + x1) // 2, wall_top, (z0 + z1) // 2, "minecraft:glowstone")
    # doorway (2-high gap) on the chosen side, centered
    cx, cz = (x0 + x1) // 2, (z0 + z1) // 2
    if door_side == "-z":   dx, dz = cx, z0
    elif door_side == "+z": dx, dz = cx, z1
    elif door_side == "-x": dx, dz = x0, cz
    else:                   dx, dz = x1, cz
    setb(dx, 64, dz, AIR); setb(dx, 65, dz, AIR)

def build_farm(x0, z0, w=9, d=7):
    for x in range(x0, x0 + w):
        for z in range(z0, z0 + d):
            occupy(x, z)
            # a water channel down the middle row
            if z == z0 + d // 2:
                setb(x, 63, z, "minecraft:water")
            else:
                setb(x, 63, z, "minecraft:farmland[moisture=7]")
                setb(x, 64, z, "minecraft:wheat[age=7]")

def build_tree(x, z):
    # trunk
    for y in range(64, 69):
        setb(x, y, z, "minecraft:oak_log[axis=y]")
    occupy(x, z)
    # canopy
    for y in (67, 68):
        for dx in range(-2, 3):
            for dz in range(-2, 3):
                if abs(dx) == 2 and abs(dz) == 2:
                    continue
                if dx == 0 and dz == 0 and y == 67:
                    continue
                setb(x + dx, y, z + dz, "minecraft:oak_leaves")
    for dx in range(-1, 2):
        for dz in range(-1, 2):
            setb(x + dx, 69, z + dz, "minecraft:oak_leaves")
    setb(x, 70, z, "minecraft:oak_leaves")

def h2(x, z):
    return ((x * 73856093) ^ (z * 19349663)) & 0x7FFFFFFF

def scatter_trees():
    placed = []
    for x in range(-50, 51):
        for z in range(-50, 51):
            if (x, z) in OCCUPIED:
                continue
            if abs(x) <= 6 and abs(z) <= 6:      # keep the plaza clear
                continue
            if h2(x, z) % 53 != 0:
                continue
            if any(abs(x - px) < 5 and abs(z - pz) < 5 for px, pz in placed):
                continue
            build_tree(x, z); placed.append((x, z))
    return len(placed)

def scatter_flowers():
    flowers = ["minecraft:poppy", "minecraft:dandelion",
               "minecraft:oxeye_daisy", "minecraft:cornflower"]
    n = 0
    for x in range(-50, 51):
        for z in range(-50, 51):
            if (x, z) in OCCUPIED:
                continue
            if h2(x, z) % 13 != 0:
                continue
            if block_at(x, 64, z) != AIR:   # don't overwrite a structure
                continue
            setb(x, 64, z, flowers[h2(x, z) % len(flowers)]); n += 1
    return n

def build_village():
    carve_roads()
    # four houses around the crossroads
    build_house(6, 6, door_side="-z")
    build_house(-13, 6, door_side="-z")
    build_house(6, -13, door_side="+z")
    build_house(-13, -13, door_side="+z")
    # a fifth house further out + a farm
    build_house(18, -3, door_side="-x")
    build_farm(-26, 6)
    t = scatter_trees()
    f = scatter_flowers()
    print(f"village: 5 houses, 1 farm, {t} trees, {f} flowers")

# ---- chunk / region writers ----
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
        bits = max(4, (len(pal) - 1).bit_length())
        per = 64 // bits
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

def affected_chunks():
    cs = set()
    for (x, _, z) in OVERRIDES:
        cs.add((x >> 4, z >> 4))
    return cs

def write_region(path, chunk_bytes_by_slot, plain):
    SECTOR = 4096
    locations = bytearray(4096); timestamps = bytearray(4096); body = bytearray()
    cur = 2
    for slot in range(1024):
        comp = chunk_bytes_by_slot.get(slot, plain)
        rec = struct.pack(">I", len(comp) + 1) + b"\x02" + comp
        rec += b"\x00" * ((-len(rec)) % SECTOR)
        n = len(rec) // SECTOR
        struct.pack_into(">I", locations, slot * 4, (cur << 8) | (n & 0xFF))
        struct.pack_into(">I", timestamps, slot * 4, 1)
        body += rec; cur += n
    with open(path, "wb") as f:
        f.write(locations); f.write(timestamps); f.write(body)

def main():
    build_village()
    out = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", "map", "region"))
    os.makedirs(out, exist_ok=True)
    plain = build_chunk_bytes(99999, 99999)   # an untouched flat chunk (reused everywhere)
    custom = affected_chunks()
    # precompute custom chunk bytes
    custom_bytes = {(cx, cz): build_chunk_bytes(cx, cz) for (cx, cz) in custom}
    for rx in (-1, 0):
        for rz in (-1, 0):
            slot_map = {}
            for cx in range(rx * 32, rx * 32 + 32):
                for cz in range(rz * 32, rz * 32 + 32):
                    if (cx, cz) in custom_bytes:
                        slot = (cx & 31) + (cz & 31) * 32
                        slot_map[slot] = custom_bytes[(cx, cz)]
            write_region(os.path.join(out, f"r.{rx}.{rz}.mca"), slot_map, plain)
    print(f"wrote 4 region files to {out}; {len(custom)} custom chunks")

if __name__ == "__main__":
    main()
