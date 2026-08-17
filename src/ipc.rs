use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde_json::{Value, json};

use pumpkin_plugin_api::common::BlockPos;
use pumpkin_plugin_api::world;

use crate::clipboard::{Clipboard, schematic_to_clipboard};
use crate::commands::is_safe_filename;
use crate::parser;
use crate::paste::schedule_block_op;
use crate::{
    ACTIVE_PASTES, PLUGIN_VERSION, debug_log, get_config, resolve_fallback_block, resolve_palette,
    schematics_dir,
};

const KIND_CAPABILITIES: &str = "pumpkinetica:capabilities/v1";
const KIND_LIST: &str = "pumpkinetica:list/v1";
const KIND_INFO: &str = "pumpkinetica:info/v1";
const KIND_PASTE: &str = "pumpkinetica:paste/v1";
const KIND_STATUS: &str = "pumpkinetica:status/v1";

#[derive(Deserialize)]
struct IpcEnvelope {
    kind: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
struct SchematicReq {
    schematic: String,
}

#[derive(Deserialize)]
struct PasteReq {
    schematic: String,
    x: i32,
    y: i32,
    z: i32,
    world: String,
}

/// Entry point for `Plugin::handle_ipc_message`. Decodes the envelope, dispatches
/// by `kind`, and returns a JSON reply. Every failure path is a clean `Err(String)`
/// so a malformed request can never abort the WASM instance.
pub(crate) fn dispatch(sender: &str, message: &[u8]) -> Result<Vec<u8>, String> {
    let env: IpcEnvelope =
        serde_json::from_slice(message).map_err(|e| format!("bad envelope: {e}"))?;
    debug_log(&format!("ipc from '{sender}': {}", env.kind));

    let reply = match env.kind.as_str() {
        KIND_CAPABILITIES => json!({
            "version": PLUGIN_VERSION,
            "kinds": [KIND_CAPABILITIES, KIND_LIST, KIND_INFO, KIND_PASTE, KIND_STATUS],
        }),
        KIND_LIST => op_list()?,
        KIND_INFO => op_info(env.payload)?,
        KIND_PASTE => op_paste(sender, env.payload)?,
        KIND_STATUS => op_status(),
        unknown => return Err(format!("unsupported kind: {unknown}")),
    };

    serde_json::to_vec(&reply).map_err(|e| format!("encode reply: {e}"))
}

fn op_list() -> Result<Value, String> {
    let dir = schematics_dir().ok_or("plugin not initialized")?;
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("read dir: {e}"))?;
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    Ok(json!({ "schematics": names }))
}

fn op_info(payload: Value) -> Result<Value, String> {
    let req: SchematicReq = serde_json::from_value(payload).map_err(|e| format!("payload: {e}"))?;
    let (clip, regions, unresolved) = load_clip(&req.schematic)?;
    let blocks = clip.blocks.iter().filter(|&&b| b != 0).count();
    Ok(json!({
        "name": clip.name,
        "regions": regions,
        "blocks": blocks,
        "unresolved": unresolved,
    }))
}

fn op_paste(sender: &str, payload: Value) -> Result<Value, String> {
    let req: PasteReq = serde_json::from_value(payload).map_err(|e| format!("payload: {e}"))?;

    let config = get_config();
    if ACTIVE_PASTES.load(Ordering::Relaxed) >= config.max_concurrent_pastes {
        return Err("server busy: max concurrent pastes reached".into());
    }

    let (clip, _regions, _unresolved) = load_clip(&req.schematic)?;
    let origin = BlockPos {
        x: req.x,
        y: req.y,
        z: req.z,
    };
    let (queue, tile_entities) = clip.to_work_queue(origin);
    let total = queue.len();
    let name = clip.name.clone();

    debug_log(&format!(
        "ipc paste '{name}' by {sender}: {total} block(s) in '{}' at ({}, {}, {})",
        req.world, req.x, req.y, req.z
    ));

    schedule_block_op(
        queue,
        tile_entities,
        req.world,
        config.blocks_per_tick,
        format!("ipc:{sender}"),
        format!("IPC paste '{name}' at ({}, {}, {})", req.x, req.y, req.z),
        false,
    );

    Ok(json!({ "accepted": true, "blocks": total }))
}

fn op_status() -> Value {
    let config = get_config();
    json!({
        "active_pastes": ACTIVE_PASTES.load(Ordering::Relaxed),
        "max_concurrent": config.max_concurrent_pastes,
    })
}

/// Read a schematic file from the schematics dir and turn it into a clipboard,
/// mirroring the `/schematic load` pipeline but without any player context.
/// Returns the clipboard, region count, and count of block types that did not
/// natively resolve.
fn load_clip(file: &str) -> Result<(Clipboard, usize, usize), String> {
    if !is_safe_filename(file) {
        return Err("invalid file name".into());
    }
    let dir = schematics_dir().ok_or("plugin not initialized")?;
    let path = format!("{dir}/{file}");
    let data = std::fs::read(&path).map_err(|e| format!("read file: {e}"))?;
    let schematic = parser::parse_schematic(&data, file).map_err(|e| format!("parse: {e}"))?;

    let config = get_config();
    let fallback = resolve_fallback_block(&config);
    let palette_map: Vec<Vec<Option<u16>>> = schematic
        .regions
        .iter()
        .map(|r| resolve_palette(&r.palette, fallback))
        .collect();

    let mut unresolved = 0usize;
    for region in &schematic.regions {
        for entry in &region.palette {
            if entry.name == "minecraft:air" || entry.name == "air" {
                continue;
            }
            let props: Vec<(String, String)> = entry
                .properties
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if world::resolve_block_state(&entry.name, &props).is_none() {
                unresolved += 1;
            }
        }
    }

    let regions = schematic.regions.len();
    let clip = schematic_to_clipboard(schematic.name.clone(), &schematic, &palette_map)?;
    Ok((clip, regions, unresolved))
}
