//! Crash-safe persistence of runtime block edits as an overlay on the
//! read-only base world.
//!
//! The base world (region files) is never modified. Every persistent block
//! change made at runtime (player builds, AI builds) is recorded here as a
//! compact `(x, y, z, state)` record and, on a timer + at shutdown,
//! snapshotted to `<dir>/edits.bin`.
//!
//! The snapshot is published **atomically**: write a temp file → `fsync` it →
//! `rename()` over the live file → `fsync` the directory. POSIX `rename` is
//! atomic, so a crash mid-save can never corrupt the live snapshot — a restart
//! always sees either the whole old file or the whole new one. Bounded loss is
//! the window since the last successful snapshot (a write-ahead log closes that
//! gap; see [`EditStore`] follow-up work).
//!
//! On load the overlay is replayed onto each chunk as it streams in, so edits
//! survive restarts without touching the base map.

use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use glam::{I16Vec2, IVec3};
use tracing::{info, warn};
use valence_generated::block::BlockState;

/// File header magic + version.
const MAGIC: &[u8; 8] = b"HYPEDIT1";
/// Bytes per edit record: i32 x, i32 y, i32 z, u16 raw-state.
const RECORD: usize = 14;

fn chunk_of(pos: IVec3) -> I16Vec2 {
    I16Vec2::new((pos.x >> 4) as i16, (pos.z >> 4) as i16)
}

/// Game-thread-only overlay of block edits, keyed by chunk for fast replay.
pub struct EditStore {
    dir: PathBuf,
    by_chunk: HashMap<I16Vec2, HashMap<IVec3, BlockState>>,
    dirty: bool,
}

impl EditStore {
    /// Open (and load) the overlay in `dir`, creating the directory if needed.
    pub fn open(dir: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(&dir)?;
        let mut by_chunk: HashMap<I16Vec2, HashMap<IVec3, BlockState>> = HashMap::new();
        let path = dir.join("edits.bin");
        let loaded = match fs::read(&path) {
            Ok(buf) => Self::decode_into(&buf, &mut by_chunk),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        info!("world persistence: loaded {loaded} block edits from {}", path.display());
        Ok(Self {
            dir,
            by_chunk,
            dirty: false,
        })
    }

    fn decode_into(
        buf: &[u8],
        out: &mut HashMap<I16Vec2, HashMap<IVec3, BlockState>>,
    ) -> usize {
        if buf.len() < 16 || &buf[0..8] != MAGIC {
            warn!("edits.bin missing header/bad magic; ignoring");
            return 0;
        }
        let count = u64::from_le_bytes(buf[8..16].try_into().unwrap()) as usize;
        let mut off = 16;
        let mut n = 0;
        for _ in 0..count {
            if off + RECORD > buf.len() {
                warn!("edits.bin truncated; loaded {n} of {count} edits");
                break;
            }
            let x = i32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
            let y = i32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
            let z = i32::from_le_bytes(buf[off + 8..off + 12].try_into().unwrap());
            let raw = u16::from_le_bytes(buf[off + 12..off + 14].try_into().unwrap());
            off += RECORD;
            if let Some(state) = BlockState::from_raw(raw) {
                let pos = IVec3::new(x, y, z);
                out.entry(chunk_of(pos)).or_default().insert(pos, state);
                n += 1;
            }
        }
        n
    }

    /// Record one edit (latest write per position wins).
    pub fn record(&mut self, pos: IVec3, state: BlockState) {
        self.by_chunk
            .entry(chunk_of(pos))
            .or_default()
            .insert(pos, state);
        self.dirty = true;
    }

    /// Edits for a chunk, to replay when it loads.
    pub fn for_chunk(&self, chunk: I16Vec2) -> Option<&HashMap<IVec3, BlockState>> {
        self.by_chunk.get(&chunk)
    }

    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Serialize the full overlay to bytes (fast, in-memory) and clear the dirty
    /// flag. Pair with [`write_atomic`] to publish the bytes durably.
    pub fn serialize_and_clear(&mut self) -> Vec<u8> {
        let total: usize = self.by_chunk.values().map(HashMap::len).sum();
        let mut buf = Vec::with_capacity(16 + total * RECORD);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&(total as u64).to_le_bytes());
        for chunk in self.by_chunk.values() {
            for (pos, state) in chunk {
                buf.extend_from_slice(&pos.x.to_le_bytes());
                buf.extend_from_slice(&pos.y.to_le_bytes());
                buf.extend_from_slice(&pos.z.to_le_bytes());
                buf.extend_from_slice(&state.to_raw().to_le_bytes());
            }
        }
        self.dirty = false;
        buf
    }
}

/// Atomically publish `buf` as `<dir>/edits.bin`: temp file → fsync → rename →
/// fsync dir. Safe to call off the game thread.
pub fn write_atomic(dir: &Path, buf: &[u8]) -> anyhow::Result<()> {
    let final_path = dir.join("edits.bin");
    let tmp_path = dir.join("edits.bin.tmp");
    {
        let mut f = File::create(&tmp_path)?;
        f.write_all(buf)?;
        f.sync_all()?; // data + metadata on disk before we publish
    }
    fs::rename(&tmp_path, &final_path)?; // atomic publish
    // Best-effort fsync of the directory so the rename itself is durable.
    if let Ok(dirf) = File::open(dir) {
        if let Err(e) = dirf.sync_all() {
            warn!("failed to fsync edits dir: {e}");
        }
    }
    Ok(())
}
