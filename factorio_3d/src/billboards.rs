// per-entity billboard rects.
//
// while the 3d view is active, the sprite-placement hook records the exact
// on-screen rect of every object sprite. once per frame the finished set is
// published into a per-chunk store (factorio caches draw queues per chunk,
// so only changed chunks show up each frame — a whole-frame replace would
// lose everything cached). the renderer turns each stored rect into one
// standing quad.

use crate::settings::MAX_RECTS;
use std::collections::HashMap;
use std::sync::Mutex;

// one sprite part's exact on-screen rect, in fbo pixels
#[derive(Clone, Copy)]
pub struct BbRect {
    pub cx: f32,
    pub cy: f32,
    pub hw: f32,
    pub hh: f32,
    // grouping key: the raw MapPosition ints (same for all parts of an entity)
    pub kx: i32,
    pub ky: i32,
    // player sprite (screen-bottom anchored, hidden in first person)
    pub player: bool,
    // enemy/unit sprite (screen-bottom anchored like the player)
    pub unit: bool,
    // lay flat on the ground instead of standing up (thrusters etc.)
    pub flat: bool,
    // view rect (left, top, span_x, span_y in tiles) this rect was placed
    // under — used to remap old rects exactly into the current view
    pub stamp: (f32, f32, f32, f32),
    // mobile entity (player/vehicle): re-recorded every frame, lives in the
    // per-frame overlay instead of the chunk store
    pub special: bool,
    // entity serial for special sprites (0 = none) — exact per-entity grouping
    pub grp: u32,
}

// one chunk's rects plus the view rect they were recorded under
#[derive(Clone)]
pub struct ChunkBatch {
    pub stamp: (f32, f32, f32, f32),
    pub rects: Vec<BbRect>,
}

// this frame's in-progress recordings (drained at the frame boundary)
static PENDING: Mutex<Vec<BbRect>> = Mutex::new(Vec::new());
// persistent per-chunk store of finished batches
static STORE: Mutex<Option<HashMap<(i32, i32), ChunkBatch>>> = Mutex::new(None);
// mobile entities (player/vehicles) — replaced wholesale every frame
static SPECIAL_OVERLAY: Mutex<Option<ChunkBatch>> = Mutex::new(None);
// view rect of the previous publish, for jump detection
static LAST_STAMP: Mutex<Option<(f32, f32, f32, f32)>> = Mutex::new(None);

// record one placed sprite rect (called from the placement hook)
pub fn record(rect: BbRect) {
    if let Ok(mut v) = PENDING.lock() {
        if v.len() < MAX_RECTS {
            v.push(rect);
        }
    }
}

// chunk key from the raw 1/256-tile MapPosition: tile>>5 == raw>>13
#[inline]
fn chunk_key(r: &BbRect) -> (i32, i32) {
    (r.kx >> 13, r.ky >> 13)
}

// drop everything (used when the boost engages and on view jumps — the full
// re-queue rebuilds the store within a frame)
pub fn clear_store() {
    if let Ok(mut g) = STORE.lock() {
        if let Some(s) = g.as_mut() {
            s.clear();
        }
    }
    if let Ok(mut v) = PENDING.lock() {
        v.clear();
    }
    if let Ok(mut ov) = SPECIAL_OVERLAY.lock() {
        *ov = None;
    }
}

// keep only rects whose own placement stamp matches the most common one and
// return that stamp. stragglers from a neighboring frame (the game pipelines
// its prepare work) are dropped for one frame instead of poisoning the batch.
fn majority_stamp(
    rects: Vec<BbRect>,
    fallback: (f32, f32, f32, f32),
) -> ((f32, f32, f32, f32), Vec<BbRect>) {
    let key = |s: &(f32, f32, f32, f32)| -> (i32, i32, i32, i32) {
        ((s.0 * 8.0) as i32, (s.1 * 8.0) as i32, (s.2 * 8.0) as i32, (s.3 * 8.0) as i32)
    };
    let mut counts: HashMap<(i32, i32, i32, i32), u32> = HashMap::new();
    for r in &rects {
        if r.stamp.2 > 0.1 {
            *counts.entry(key(&r.stamp)).or_insert(0) += 1;
        }
    }
    let best = match counts.into_iter().max_by_key(|(_, n)| *n) {
        Some((k, _)) => k,
        None => return (fallback, rects),
    };
    let mut stamp = fallback;
    let rects: Vec<BbRect> = rects
        .into_iter()
        .filter(|r| {
            if r.stamp.2 > 0.1 && key(&r.stamp) == best {
                stamp = r.stamp;
                true
            } else {
                false
            }
        })
        .collect();
    (stamp, rects)
}

// frame boundary (start of createRenderParameters, before the new frame's
// queues exist): everything recorded since the last call is complete —
// upsert it into the chunk store and evict chunks that left the view
pub fn publish_frame() {
    crate::capture::advance_hi_rotation();

    let fresh = match PENDING.lock() {
        Ok(mut v) => std::mem::take(&mut *v),
        Err(_) => Vec::new(),
    };
    let stamp = crate::hooks::frame::view_rect_tiles();
    let mut guard = match STORE.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let store = guard.get_or_insert_with(HashMap::new);

    // view jump (remote view, teleport, map hop): old batches would remap
    // wildly — drop everything, the forced re-queue rebuilds it this frame
    {
        let (l, t, sx, sy) = stamp;
        let last = LAST_STAMP.lock().ok().and_then(|g| *g);
        if let Some((ll, lt, lsx, lsy)) = last {
            if sx > 0.1 && lsx > 0.1 {
                let dcx = ((l + sx * 0.5) - (ll + lsx * 0.5)).abs();
                let dcy = ((t + sy * 0.5) - (lt + lsy * 0.5)).abs();
                let ratio = (sx / lsx).max(lsx / sx);
                if dcx > 0.25 * sx || dcy > 0.25 * sy || ratio > 1.3 {
                    store.clear();
                    if let Ok(mut ov) = SPECIAL_OVERLAY.lock() {
                        *ov = None;
                    }
                    log::info!("[billboards] view jump — store cleared");
                }
            }
        }
        if sx > 0.1 {
            if let Ok(mut g) = LAST_STAMP.lock() {
                *g = Some(stamp);
            }
        }
    }

    if !fresh.is_empty() {
        // mobile entities go to the per-frame overlay, static ones per chunk
        let (specials, normals): (Vec<BbRect>, Vec<BbRect>) =
            fresh.into_iter().partition(|r| r.special);

        let (sp_stamp, specials) = majority_stamp(specials, stamp);
        if let Ok(mut ov) = SPECIAL_OVERLAY.lock() {
            *ov = Some(ChunkBatch { stamp: sp_stamp, rects: specials });
        }

        let mut by_chunk: HashMap<(i32, i32), Vec<BbRect>> = HashMap::new();
        for r in normals {
            by_chunk.entry(chunk_key(&r)).or_default().push(r);
        }
        for (key, rects) in by_chunk {
            let (bstamp, rects) = majority_stamp(rects, stamp);
            if !rects.is_empty() {
                store.insert(key, ChunkBatch { stamp: bstamp, rects });
            }
        }
    }

    // evict chunks well outside the current view so the store stays bounded
    let (l, t, sx, sy) = stamp;
    if sx > 0.1 && sy > 0.1 {
        store.retain(|&(cx, cy), _| {
            let (x0, x1) = (cx as f32 * 32.0, cx as f32 * 32.0 + 32.0);
            let (y0, y1) = (cy as f32 * 32.0, cy as f32 * 32.0 + 32.0);
            x1 > l - 40.0 && x0 < l + sx + 40.0 && y1 > t - 40.0 && y0 < t + sy + 40.0
        });
    }
}

// snapshot of all stored batches (each carries its own stamp; the renderer
// remaps every batch into the current view, so batch age doesn't matter)
pub fn take_batches() -> Vec<ChunkBatch> {
    let mut batches: Vec<ChunkBatch> = STORE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.values().cloned().collect()))
        .unwrap_or_default();
    if let Ok(ov) = SPECIAL_OVERLAY.lock() {
        if let Some(b) = ov.as_ref() {
            if !b.rects.is_empty() {
                batches.push(b.clone());
            }
        }
    }
    batches
}
