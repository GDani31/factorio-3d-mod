// item hooks: everything that puts a 3d item model on screen.
//
// paths an item can draw through (2.0):
// - belt items: TransportLine::draw -> drawScaledTinted (fast sprite path),
//   mapped back to the item via a prebuilt belt-sprite -> model table
// - stacked/qualified items: ItemStack::drawInternal (id read from the stack)
// - ground items: ItemEntity::draw, adopted into the entity registry via the
//   nested drawInternal so they persist + clean up like any entity
// - inserter hands: read straight from the inserter's fields (machines.rs
//   calls record_inserter_hand while the vanilla draw is suppressed)

use crate::hooks::mem::{self, dq_is_ghost, readable};
use crate::hooks::{MapPos, getters, hook};
use crate::offsets;
use crate::symbols::SymbolMap;
use crate::util::memo;
use anyhow::Result;
use retour::static_detour;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

static_detour! {
    static ItemEntityDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    // drawItems(this, DrawQueue&, TransportLineConnector const&, uint&, FixedPoint&)
    static TbDrawItemsHook: unsafe extern "C" fn(usize, usize, usize, usize, usize);
    static UbDrawItemsHook: unsafe extern "C" fn(usize, usize, usize, usize, usize);
    static SplitterDrawItemsHook: unsafe extern "C" fn(usize, usize, usize, usize, usize);
    // Item::drawItemOnMap(DrawQueue&, MapPosition const&, RenderLayer, char, Item const&)
    static DrawItemOnMapHook: unsafe extern "C" fn(usize, usize, usize, usize, usize);
    // ItemStack::drawInternal(DrawQueue&, MapPosition const&, uchar, RenderLayer,
    //   RenderLayer, char, FixedPoint)
    static DrawInternalHook: unsafe extern "C" fn(usize, usize, usize, usize, usize, usize, usize, usize);
    // DrawQueue::drawScaledTinted(Sprite const&, MapPosition const&, double,
    //   double, Color, DrawingFlags, RenderLayer, Vector const&, char)
    static ScaledTintedHook: unsafe extern "C" fn(
        usize, usize, usize, f64, f64, usize, usize, usize, usize, usize);
    // ItemStackPainter::drawItemStack — first 4 args are integer-class (queue,
    // id, sprite, MapPosition by value); the rest ride the stack and forward
    // blindly as usize
    static DrawItemStackHook: unsafe extern "C" fn(
        usize, usize, usize, usize, usize, usize,
        usize, usize, usize, usize, usize, usize);
}

thread_local! {
    // ground-item entity currently drawing (for item adoption)
    static CURRENT_ITEM: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    // set while inside ItemStack::drawInternal — drawItemStack fires nested
    // from there (belt/ground items) and must NOT double-record; a direct
    // drawItemStack call (flag clear) is an inserter hand
    static IN_DRAW_INTERNAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // set while a belt draws its items — the fast drawScaledTinted sprite
    // path only records belt items while this is set
    static IN_BELT_DRAW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// address of the game's global Item* table pointer (indexed by item id) —
// parsed at install time from drawInternal's `mov rax,[rip+disp]`
static ITEM_TABLE_PTR: AtomicUsize = AtomicUsize::new(0);
static TBC_DIR_ADDR: AtomicUsize = AtomicUsize::new(0);

// fire counters, only for the periodic diagnostics in the log
static ITEM_HOOK_FIRES: AtomicUsize = AtomicUsize::new(0); // drawItemOnMap
static STACK_HOOK_FIRES: AtomicUsize = AtomicUsize::new(0); // ItemStack::drawInternal
static ISTACK_FIRES: AtomicUsize = AtomicUsize::new(0); // ItemStackPainter::drawItemStack
static TB_ITEMS_FIRES: AtomicUsize = AtomicUsize::new(0);
static INSERTER_ITEM_LOG: AtomicUsize = AtomicUsize::new(0);

// --- item prototype -> model key ------------------------------------------------------

static ITEM_PROTO_CACHE: LazyLock<Mutex<HashMap<usize, Option<&'static str>>>> =
    LazyLock::new(Default::default);
static ITEM_PROTOS_LOGGED: AtomicUsize = AtomicUsize::new(0);

fn resolve_from_strings(strings: &[(usize, String)]) -> Option<&'static str> {
    strings.iter().find_map(|(_, s)| {
        if s.contains('.') || s.contains('[') || s.contains('/') {
            return None;
        }
        crate::models::resolve_item(s)
    })
}

fn item_proto_key(item: usize) -> Option<&'static str> {
    memo(&ITEM_PROTO_CACHE, item, || {
        // the item name lives directly on the prototype, or one pointer hop away
        // (some runtime item objects hold a prototype pointer in a leading field)
        let mut strings = mem::scan_proto_strings(item as *const u8);
        let mut key = resolve_from_strings(&strings);
        if key.is_none() {
            for off in [0usize, 8, 0x10, 0x18] {
                let Some(sub) = mem::try_read::<usize>(item + off) else { continue };
                if sub == 0 || sub == item || !readable(sub as *const u8, 0x40) {
                    continue;
                }
                let s2 = mem::scan_proto_strings(sub as *const u8);
                if let Some(k) = resolve_from_strings(&s2) {
                    strings = s2;
                    key = Some(k);
                    break;
                }
            }
        }
        if ITEM_PROTOS_LOGGED.fetch_add(1, Ordering::Relaxed) < 20 {
            log::info!("[items] proto @0x{item:X}: strings {strings:?} -> {key:?}");
        }
        key
    })
}

// item id -> model key via the game's global item prototype table
fn item_key_by_id(id: usize) -> Option<&'static str> {
    let table_ptr = ITEM_TABLE_PTR.load(Ordering::Relaxed);
    if table_ptr == 0 || id == 0 {
        return None;
    }
    let table: usize = mem::read(table_ptr);
    if table == 0 {
        return None;
    }
    let item: usize = mem::try_read(table + id * 8)?;
    if item == 0 || !readable(item as *const u8, 0x40) {
        return None;
    }
    item_proto_key(item)
}

fn record_item_at(key: &'static str, pos: usize) {
    let (x, y) = unsafe { MapPos::tiles_at(pos) };
    crate::entities::record_item(key, x, y);
}

// --- belt sprite -> item model table --------------------------------------------------

// belt-sprite variation pointer -> item model key. built once from the item
// table: each item prototype exposes its on-belt sprites in [+0x368, +0x370)
// with a 0xA0 stride; the fast belt path hands drawScaledTinted one of those
// variation pointers, which we map straight back to the item's model.
// write-once, read-many: built during the first belt draw, then read on the
// hot per-sprite path with NO lock (Factorio's parallel render threads were
// contending 2.4M times/session on the old Mutex -> stalls)
static BELT_SPRITE_MAP: OnceLock<crate::util::FxHashMap<usize, &'static str>> = OnceLock::new();
static ST_BELT_HIT: AtomicUsize = AtomicUsize::new(0);
static ST_BELT_MISS: AtomicUsize = AtomicUsize::new(0);

fn ensure_belt_sprite_map() {
    if BELT_SPRITE_MAP.get().is_some() {
        return;
    }
    let table_ptr = ITEM_TABLE_PTR.load(Ordering::Relaxed);
    if table_ptr == 0 {
        return; // table not parsed yet — retry next belt draw, don't cache empty
    }
    let mut map = crate::util::FxHashMap::default();
    let mut resolved = 0usize; // items with a model (indexed one way or another)
    let mut with_pictures = 0usize; // subset that also have the belt-pictures array
    let table: usize = mem::read(table_ptr);
    for id in 1..2048usize {
        if !readable((table + id * 8) as *const u8, 8) {
            break;
        }
        let proto: usize = mem::read(table + id * 8);
        if proto == 0 || !readable((proto + offsets::ITEM_PROTO_SPRITE_END + 8) as *const u8, 8) {
            continue;
        }
        let Some(key) = item_key_by_id(id) else { continue };
        resolved += 1;

        // icon sprite — the path MOST items take on belts (plates, pipes,
        // building parts): the on-belt Sprite is at *(proto+0x2B8)+0x68
        if let Some(icon_base) = mem::try_read::<usize>(proto + offsets::ITEM_PROTO_ICON_FIELD) {
            let sprite = icon_base + offsets::ITEM_PROTO_ICON_SPRITE_OFF;
            if icon_base != 0 && readable(sprite as *const u8, 0x10) {
                map.insert(sprite, key);
            }
        }

        // belt-pictures variation array — only ~7 items (coal, ores) have it
        let base: usize = mem::read(proto + offsets::ITEM_PROTO_SPRITE_BASE);
        let end: usize = mem::read(proto + offsets::ITEM_PROTO_SPRITE_END);
        if base != 0 && end > base && (end - base) % offsets::ITEM_SPRITE_STRIDE == 0 {
            let count = (end - base) / offsets::ITEM_SPRITE_STRIDE;
            if (1..=128).contains(&count) && readable(base as *const u8, offsets::ITEM_SPRITE_STRIDE)
            {
                with_pictures += 1;
                for v in 0..count {
                    map.insert(base + v * offsets::ITEM_SPRITE_STRIDE, key);
                }
            }
        }
    }
    log::info!(
        "[items] belt sprite map: {} sprite entries covering {} items with a model ({} also have a belt-pictures array)",
        map.len(),
        resolved,
        with_pictures
    );
    let _ = BELT_SPRITE_MAP.set(map);
}

// locate the global item table: drawInternal's prologue contains
// `mov rax, [rip+disp]` (48 8B 05 xx xx xx xx) loading the table pointer
fn parse_item_table(draw_internal: usize) {
    if !readable(draw_internal as *const u8, 0x40) {
        return;
    }
    let code = unsafe { std::slice::from_raw_parts(draw_internal as *const u8, 0x40) };
    for i in 0..code.len() - 7 {
        if code[i] == 0x48 && code[i + 1] == 0x8B && code[i + 2] == 0x05 {
            let disp =
                i32::from_le_bytes([code[i + 3], code[i + 4], code[i + 5], code[i + 6]]) as isize;
            let addr = (draw_internal + i + 7).wrapping_add_signed(disp);
            ITEM_TABLE_PTR.store(addr, Ordering::Relaxed);
            log::info!("[items] item table pointer @0x{addr:X}");
            return;
        }
    }
    log::warn!("[items] item table pattern not found in drawInternal — belt items stay 2d");
}

// --- the hooks -------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn hooked_draw_item_stack(
    dq: usize, id: usize, sprite: usize, pos: usize, a5: usize, a6: usize,
    a7: usize, a8: usize, a9: usize, a10: usize, a11: usize, a12: usize,
) {
    // a direct call (not nested in drawInternal) is an inserter hand: the
    // id (arg 2, low 16 bits) indexes the global item table
    let nested = IN_DRAW_INTERNAL.with(|c| c.get());
    let n = ISTACK_FIRES.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        log::info!("[items] drawItemStack path active");
    }
    if !nested && pos != 0 && !dq_is_ghost(dq) {
        let key = item_key_by_id(id & 0xFFFF);
        if n < 30 {
            log::info!("[items] drawItemStack id {} -> {key:?}", id & 0xFFFF);
        }
        if let Some(key) = key {
            let (px, py) = super::MapPos::tiles_packed(pos as u64);
            crate::entities::record_item(key, px, py);
        }
    }
    // vanilla item sprite always draws too — it's hidden under the 3d belt
    unsafe { DrawItemStackHook.call(dq, id, sprite, pos, a5, a6, a7, a8, a9, a10, a11, a12) }
}

// ground items: first draw runs the original under a marker so the inner
// drawItemStack can identify + adopt the item; later draws suppress the
// sprite once the model is ready
fn hooked_item_entity_draw(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
    if this.is_null() || dq_is_ghost(queue as usize) {
        unsafe { ItemEntityDrawHook.call(this, queue) };
        return;
    }
    if let Some(model) = crate::entities::model_of(this as usize) {
        if let Some((x, y)) = getters::entity_pos(this) {
            let suppress = crate::entities::record(crate::entities::Record {
                surface: getters::entity_surface(this),
                ..crate::entities::Record::at(this as usize, &model, x, y)
            });
            if suppress {
                return;
            }
        }
        unsafe { ItemEntityDrawHook.call(this, queue) };
        return;
    }
    CURRENT_ITEM.with(|c| c.set(this as usize));
    unsafe { ItemEntityDrawHook.call(this, queue) };
    CURRENT_ITEM.with(|c| c.set(0));
}

// pure recorder for the direct Item& map path (rarely used; belts go through
// drawInternal). guarded so it never double-records a drawInternal item
fn hooked_draw_item_on_map(dq: usize, pos: usize, layer: usize, ch: usize, item: usize) {
    let n = ITEM_HOOK_FIRES.fetch_add(1, Ordering::Relaxed);
    if n == 0 || n % 3600 == 0 {
        log::info!("[items] drawItemOnMap fires: {}", n + 1);
    }
    if !IN_DRAW_INTERNAL.with(|c| c.get())
        && pos != 0
        && item != 0
        && readable(item as *const u8, 0x40)
        && !dq_is_ghost(dq)
    {
        if let Some(key) = item_proto_key(item) {
            record_item_at(key, pos);
        }
    }
    unsafe { DrawItemOnMapHook.call(dq, pos, layer, ch, item) }
}

// DrawQueue::drawScaledTinted — hot generic sprite draw; we only act while a
// belt is drawing its items, mapping the sprite back to its 3d item model
#[allow(clippy::too_many_arguments)]
fn hooked_scaled_tinted(
    dq: usize, sprite: usize, pos: usize, s1: f64, s2: f64, color: usize,
    flags: usize, layer: usize, vec: usize, ch: usize,
) {
    if IN_BELT_DRAW.with(|c| c.get()) && pos != 0 && !dq_is_ghost(dq) {
        let key = BELT_SPRITE_MAP.get().and_then(|m| m.get(&sprite).copied());
        // count belt-sprite hits vs misses: if we get lots of in-belt draws
        // but all miss, the sprite map is stale/sparse (needs rebuild)
        let h = ST_BELT_HIT.load(Ordering::Relaxed);
        let m = ST_BELT_MISS.load(Ordering::Relaxed);
        if key.is_some() {
            ST_BELT_HIT.fetch_add(1, Ordering::Relaxed);
        } else {
            ST_BELT_MISS.fetch_add(1, Ordering::Relaxed);
        }
        if (h + m) % 3600 == 0 {
            log::info!("[items] belt scaledTinted hits={} miss={} (miss sprite@0x{sprite:X})", h, m);
        }
        if let Some(key) = key {
            record_item_at(key, pos);
        }
    }
    unsafe { ScaledTintedHook.call(dq, sprite, pos, s1, s2, color, flags, layer, vec, ch) }
}

// ItemStack::drawInternal — how map items draw (belts + ground). the stack
// holds an item id (u16 at +4); the Item* comes from the global table
#[allow(clippy::too_many_arguments)]
fn hooked_draw_internal(
    this: usize, dq: usize, pos: usize, e: usize, layer: usize, layer2: usize,
    ch: usize, fixed: usize,
) {
    let n = STACK_HOOK_FIRES.fetch_add(1, Ordering::Relaxed);
    if n == 0 || n % 3600 == 0 {
        log::info!("[items] ItemStack::drawInternal fires: {}", n + 1);
    }
    if this != 0 && pos != 0 && readable(this as *const u8, 8) && !dq_is_ghost(dq) {
        let id = mem::read::<u16>(this + offsets::ITEMSTACK_ID) as usize;
        let key = item_key_by_id(id);
        if n < 30 {
            let ci = CURRENT_ITEM.with(|c| c.get());
            log::info!("[items] drawInternal this@0x{this:X} id {id} ground {} -> {key:?}", ci != 0);
        }
        if let Some(key) = key {
            let ground_item = CURRENT_ITEM.with(|c| c.get());
            if ground_item != 0 {
                // dropped item entity: adopt it into the registry
                let (x, y) = unsafe { MapPos::tiles_at(pos) };
                let surface = getters::entity_surface(ground_item as *mut core::ffi::c_void);
                crate::entities::adopt_item(ground_item, key, x, y, surface);
            } else {
                record_item_at(key, pos);
            }
        }
    }
    // the nested drawItemStack must not double-record
    IN_DRAW_INTERNAL.with(|c| c.set(true));
    unsafe { DrawInternalHook.call(this, dq, pos, e, layer, layer2, ch, fixed) }
    IN_DRAW_INTERNAL.with(|c| c.set(false));
}

// read the inserter's held item + arm angle straight from its fields and record
// the held item as a lifted 3d model at the hand. the whole vanilla Inserter
// ::draw stays suppressed (no 2d leak), so the hand position is reconstructed
// from the arm angle + reach — exactly the sin/cos the game itself uses
pub(crate) fn record_inserter_hand(this: *mut core::ffi::c_void, x: f32, y: f32, arm_angle: f32) {
    let Some(id) = mem::try_read::<u16>(this as usize + offsets::INSERTER_HELD_ITEM_ID) else {
        return;
    };
    let id = id as usize;
    if id == 0 {
        return; // empty hand
    }
    let Some(key) = item_key_by_id(id) else { return };
    if INSERTER_ITEM_LOG.fetch_add(1, Ordering::Relaxed) < 40 {
        log::info!("[inserter] held item id {id} -> {key} angle {arm_angle:.3}");
    }
    // hand = inserter center + reach along the arm angle (RealOrientation, a
    // fraction of a full turn; 0 points north/up-screen and increases clockwise)
    let mut reach = crate::tuning::INSERTER_REACH.get();
    // the long-handed tier picks up/drops 2 tiles out; its arm model is
    // stretched to match, so the held item rides at double reach too
    if crate::entities::model_of(this as usize)
        .is_some_and(|m| m.parts[0].key.contains("/long-handed/"))
    {
        reach *= 2.0;
    }
    let theta = arm_angle * std::f32::consts::TAU;
    let hx = x + reach * theta.sin();
    let hy = y - reach * theta.cos();
    let lift = crate::tuning::INSERTER_ITEM_LIFT.get();
    let size = crate::tuning::INSERTER_ITEM_SIZE.get();
    crate::entities::record_item_ex(key, hx, hy, lift, size);
}

// --- belt recorders --------------------------------------------------------------------
// belt surfaces render from a cached animation set, so ::draw never runs
// during chunk prep — but belts CARRYING ITEMS call drawItems every frame.
// pure recorders: register the belt, never suppress (the vanilla belt
// sprite hides under the 3d model anyway)

fn record_belt(this: usize) {
    // belts are static; skip the full record (registry + grid locks) unless
    // the refresh window elapsed — drawItems calls this every frame per belt
    if this == 0 || !crate::entities::belt_record_due(this) {
        return;
    }
    let this = this as *mut core::ffi::c_void;
    let Some(model) = getters::entity_model(this) else { return };
    let Some((x, y)) = getters::entity_pos(this) else { return };
    let _ = crate::entities::record(crate::entities::Record {
        dir: getters::entity_dir(this, TBC_DIR_ADDR.load(Ordering::Relaxed)),
        surface: getters::entity_surface(this),
        ..crate::entities::Record::at(this as usize, &model, x, y)
    });
}

// run a belt's drawItems with IN_BELT_DRAW set so the nested item draws get
// recorded as 3d items — collected per belt so this call atomically replaces
// the belt's item set in the registry (no cross-belt flicker)
fn belt_items_bracket(this: usize, f: impl FnOnce()) {
    ensure_belt_sprite_map();
    IN_BELT_DRAW.with(|c| c.set(true));
    crate::entities::begin_belt_items();
    f();
    crate::entities::end_belt_items(this);
    IN_BELT_DRAW.with(|c| c.set(false));
}

fn hooked_tb_draw_items(this: usize, dq: usize, conn: usize, a4: usize, a5: usize) {
    let n = TB_ITEMS_FIRES.fetch_add(1, Ordering::Relaxed);
    if n == 0 || n % 3600 == 0 {
        log::info!("[items] TransportBelt::drawItems fires: {}", n + 1);
    }
    record_belt(this);
    belt_items_bracket(this, || unsafe { TbDrawItemsHook.call(this, dq, conn, a4, a5) });
}

fn hooked_ub_draw_items(this: usize, dq: usize, conn: usize, a4: usize, a5: usize) {
    record_belt(this);
    belt_items_bracket(this, || unsafe { UbDrawItemsHook.call(this, dq, conn, a4, a5) });
}

fn hooked_splitter_draw_items(this: usize, dq: usize, conn: usize, a4: usize, a5: usize) {
    record_belt(this);
    belt_items_bracket(this, || unsafe { SplitterDrawItemsHook.call(this, dq, conn, a4, a5) });
}

// --- install ---------------------------------------------------------------------------

// phase 1: initialize trampolines, nothing observable yet
pub(crate) fn init(symbols: &SymbolMap, base: usize) -> Result<()> {
    unsafe {
        hook!(symbols, base, DrawItemStackHook, DRAW_ITEM_STACK, hooked_draw_item_stack);
        hook!(symbols, base, TbDrawItemsHook, TB_DRAW_ITEMS, hooked_tb_draw_items);
        hook!(symbols, base, UbDrawItemsHook, UB_DRAW_ITEMS, hooked_ub_draw_items);
        hook!(symbols, base, SplitterDrawItemsHook, SPLITTER_DRAW_ITEMS, hooked_splitter_draw_items);
        hook!(symbols, base, ItemEntityDrawHook, ITEM_ENTITY_DRAW, hooked_item_entity_draw);
        hook!(symbols, base, DrawItemOnMapHook, ITEM_DRAW_ON_MAP, hooked_draw_item_on_map);
        hook!(symbols, base, DrawInternalHook, ITEM_DRAW_INTERNAL, hooked_draw_internal);
        hook!(symbols, base, ScaledTintedHook, DRAW_SCALED_TINTED, hooked_scaled_tinted);
    }
    parse_item_table(super::resolve(symbols, base, &offsets::ITEM_DRAW_INTERNAL));
    TBC_DIR_ADDR.store(super::resolve(symbols, base, &offsets::TBC_GET_DIRECTION), Ordering::Relaxed);
    Ok(())
}

// phase 2: patch the prologues — runs with all other threads suspended
pub(crate) fn enable() -> Result<()> {
    unsafe {
        DrawItemStackHook.enable()?;
        TbDrawItemsHook.enable()?;
        UbDrawItemsHook.enable()?;
        SplitterDrawItemsHook.enable()?;
        ItemEntityDrawHook.enable()?;
        DrawItemOnMapHook.enable()?;
        DrawInternalHook.enable()?;
        ScaledTintedHook.enable()?;
    }
    Ok(())
}
