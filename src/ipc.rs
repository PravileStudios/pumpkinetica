use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde_json::{Value, json};

use pumpkin_plugin_api::common::BlockPos;
use pumpkin_plugin_api::world;

use crate::clipboard::{Clipboard, schematic_to_clipboard};
use crate::commands::is_safe_filename;
use crate::parser::{self, Schematic};
use crate::paste::schedule_block_op;
use crate::{
    ACTIVE_PASTES, PLUGIN_VERSION, PluginConfig, debug_log, get_config, resolve_fallback_block,
    resolve_palette, schematics_dir,
};

const KIND_CAPABILITIES: &str = "pumpkinetica:capabilities/v1";
const KIND_LIST: &str = "pumpkinetica:list/v1";
const KIND_INFO: &str = "pumpkinetica:info/v1";
const KIND_PASTE: &str = "pumpkinetica:paste/v1";
const KIND_STATUS: &str = "pumpkinetica:status/v1";

// Reject oversized envelopes before parsing to avoid a hostile caller forcing
// a large allocation on the tick thread.
const MAX_MESSAGE_LEN: usize = 64 * 1024;

// Stable machine-readable failure codes returned in `{ok:false, code, message}`.
// Callers branch on these; the message stays free-text for humans.
mod code {
    pub const BAD_REQUEST: &str = "BAD_REQUEST";
    pub const NOT_INITIALIZED: &str = "NOT_INITIALIZED";
    pub const BUSY: &str = "BUSY";
    pub const NOT_FOUND: &str = "NOT_FOUND";
    pub const IO: &str = "IO";
    pub const PARSE: &str = "PARSE";
    pub const INVALID_SCHEMATIC: &str = "INVALID_SCHEMATIC";
    pub const PATH_DISABLED: &str = "PATH_DISABLED";
    pub const PATH_DENIED: &str = "PATH_DENIED";
}

struct AppError {
    code: &'static str,
    message: String,
}

impl AppError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Deserialize)]
struct IpcEnvelope {
    kind: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
struct SourceReq {
    #[serde(default)]
    schematic: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
struct PasteReq {
    #[serde(default)]
    schematic: Option<String>,
    #[serde(default)]
    path: Option<String>,
    x: i32,
    y: i32,
    z: i32,
    world: String,
}

/// Entry point for `Plugin::handle_ipc_message`. Transport `Err` is reserved for
/// protocol-level failures (oversized, unparseable, unknown kind); per-op
/// failures ride the reply body as `{ok:false, code, message}`.
pub(crate) fn dispatch(sender: &str, message: &[u8]) -> Result<Vec<u8>, String> {
    if message.len() > MAX_MESSAGE_LEN {
        return Err(format!(
            "message too large: {} bytes (max {MAX_MESSAGE_LEN})",
            message.len()
        ));
    }
    let env: IpcEnvelope =
        serde_json::from_slice(message).map_err(|e| format!("bad envelope: {e}"))?;
    debug_log(&format!("ipc from '{sender}': {}", env.kind));

    let result: Result<Value, AppError> = match env.kind.as_str() {
        KIND_CAPABILITIES => Ok(json!({
            "version": PLUGIN_VERSION,
            "kinds": [KIND_CAPABILITIES, KIND_LIST, KIND_INFO, KIND_PASTE, KIND_STATUS],
            "external_paths": get_config().ipc_allow_external_paths,
        })),
        KIND_LIST => op_list(),
        KIND_INFO => op_info(env.payload),
        KIND_PASTE => op_paste(sender, env.payload),
        KIND_STATUS => Ok(op_status()),
        unknown => return Err(format!("unsupported kind: {unknown}")),
    };

    let reply = match result {
        Ok(Value::Object(mut m)) => {
            m.insert("ok".into(), Value::Bool(true));
            Value::Object(m)
        }
        Ok(other) => other,
        Err(e) => json!({ "ok": false, "code": e.code, "message": e.message }),
    };
    serde_json::to_vec(&reply).map_err(|e| format!("encode reply: {e}"))
}

fn op_list() -> Result<Value, AppError> {
    let dir = schematics_dir()
        .ok_or_else(|| AppError::new(code::NOT_INITIALIZED, "plugin not initialized"))?;
    let entries =
        std::fs::read_dir(&dir).map_err(|e| AppError::new(code::IO, format!("read dir: {e}")))?;
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    Ok(json!({ "schematics": names }))
}

fn op_info(payload: Value) -> Result<Value, AppError> {
    let req: SourceReq = serde_json::from_value(payload)
        .map_err(|e| AppError::new(code::BAD_REQUEST, format!("payload: {e}")))?;
    let config = get_config();
    let (read_path, display) =
        resolve_source(req.schematic.as_deref(), req.path.as_deref(), &config)?;
    let schematic = parse_source(&read_path, &display)?;
    let unresolved = count_unresolved(&schematic);
    let regions = schematic.regions.len();
    let clip = build_clipboard(&schematic, &config)?;
    let blocks = clip.blocks.iter().filter(|&&b| b != 0).count();
    Ok(json!({
        "name": clip.name,
        "regions": regions,
        "blocks": blocks,
        "unresolved": unresolved,
    }))
}

fn op_paste(sender: &str, payload: Value) -> Result<Value, AppError> {
    let req: PasteReq = serde_json::from_value(payload)
        .map_err(|e| AppError::new(code::BAD_REQUEST, format!("payload: {e}")))?;

    let config = get_config();
    if ACTIVE_PASTES.load(Ordering::Relaxed) >= config.max_concurrent_pastes {
        return Err(AppError::new(code::BUSY, "max concurrent pastes reached"));
    }

    let (read_path, display) =
        resolve_source(req.schematic.as_deref(), req.path.as_deref(), &config)?;
    let schematic = parse_source(&read_path, &display)?;
    let clip = build_clipboard(&schematic, &config)?;
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

/// Exactly one of `schematic` (bare name in the schematics dir) or `path`
/// (absolute host path, opt-in) must be set. Returns (read path, display name).
fn resolve_source(
    schematic: Option<&str>,
    path: Option<&str>,
    config: &PluginConfig,
) -> Result<(String, String), AppError> {
    match (schematic, path) {
        (Some(_), Some(_)) => Err(AppError::new(
            code::BAD_REQUEST,
            "specify either 'schematic' or 'path', not both",
        )),
        (None, None) => Err(AppError::new(
            code::BAD_REQUEST,
            "missing 'schematic' or 'path'",
        )),
        (Some(name), None) => {
            if !is_safe_filename(name) {
                return Err(AppError::new(code::BAD_REQUEST, "invalid file name"));
            }
            let dir = schematics_dir()
                .ok_or_else(|| AppError::new(code::NOT_INITIALIZED, "plugin not initialized"))?;
            Ok((format!("{dir}/{name}"), name.to_string()))
        }
        (None, Some(raw)) => {
            let resolved = resolve_external_path(raw, config)?;
            let display = resolved
                .rsplit(['/', '\\'])
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(&resolved)
                .to_string();
            Ok((resolved, display))
        }
    }
}

// Canonicalize resolves `..` and symlinks, so the allowlist prefix check cannot
// be defeated by traversal or a symlink escaping an allowed root.
fn resolve_external_path(raw: &str, config: &PluginConfig) -> Result<String, AppError> {
    if !config.ipc_allow_external_paths {
        return Err(AppError::new(
            code::PATH_DISABLED,
            "external paths are disabled on this server",
        ));
    }
    if raw.is_empty() || raw.contains('\0') {
        return Err(AppError::new(code::BAD_REQUEST, "invalid path"));
    }
    if config.ipc_allowed_paste_dirs.is_empty() {
        return Err(AppError::new(
            code::PATH_DENIED,
            "no allowed paste directories are configured",
        ));
    }
    let canon = std::fs::canonicalize(raw)
        .map_err(|e| AppError::new(code::NOT_FOUND, format!("resolve path: {e}")))?;
    if !canon.is_file() {
        return Err(AppError::new(code::BAD_REQUEST, "path is not a file"));
    }
    let permitted = config.ipc_allowed_paste_dirs.iter().any(|root| {
        std::fs::canonicalize(root)
            .map(|r| canon.starts_with(&r))
            .unwrap_or(false)
    });
    if !permitted {
        return Err(AppError::new(
            code::PATH_DENIED,
            "path is outside all allowed directories",
        ));
    }
    canon
        .into_os_string()
        .into_string()
        .map_err(|_| AppError::new(code::BAD_REQUEST, "path is not valid UTF-8"))
}

fn parse_source(read_path: &str, display: &str) -> Result<Schematic, AppError> {
    let data = std::fs::read(read_path)
        .map_err(|e| AppError::new(code::NOT_FOUND, format!("read file: {e}")))?;
    parser::parse_schematic(&data, display)
        .map_err(|e| AppError::new(code::PARSE, format!("parse: {e}")))
}

fn build_clipboard(schematic: &Schematic, config: &PluginConfig) -> Result<Clipboard, AppError> {
    let fallback = resolve_fallback_block(config);
    let palette_map: Vec<Vec<Option<u16>>> = schematic
        .regions
        .iter()
        .map(|r| resolve_palette(&r.palette, fallback))
        .collect();
    schematic_to_clipboard(schematic.name.clone(), schematic, &palette_map)
        .map_err(|e| AppError::new(code::INVALID_SCHEMATIC, e))
}

fn count_unresolved(schematic: &Schematic) -> usize {
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
    unresolved
}
