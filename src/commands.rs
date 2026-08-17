use std::sync::atomic::Ordering;

use pumpkin_plugin_api::commands::CommandHandler;
use pumpkin_plugin_api::{
    Result, Server,
    command::{CommandError, CommandSender, ConsumedArgs},
    command_wit::Arg,
    common::{BlockPos, Hand, NamedColor},
    player::ItemStack,
    text::TextComponent,
    world,
};

use crate::clipboard::{
    Clipboard, FlipAxis, clipboard_to_schem_data, flip_clipboard, read_selection_chunk_batched,
    rotate_clipboard, schematic_to_clipboard,
};
use crate::history::PlayerHistory;
use crate::parser;
use crate::paste::{BlockPlacement, schedule_block_op, schedule_paste};
use crate::selection::Selection;
use crate::{
    ACTIVE_PASTES, PLAYER_CLIPBOARDS, PLAYER_HISTORIES, PLAYER_SELECTIONS, PLUGIN_VERSION,
    debug_log, get_config, msg_error, msg_info, msg_success, msg_warn, resolve_fallback_block,
    resolve_palette,
};

pub(crate) fn is_safe_filename(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name.contains('\0')
}

// Pull a single-word argument, or fail with the given hint.
fn simple_arg(args: &ConsumedArgs, key: &str, expected: &str) -> Result<String, CommandError> {
    match args.get_value(key) {
        Arg::Simple(s) => Ok(s),
        _ => Err(CommandError::InvalidConsumption(Some(expected.into()))),
    }
}

// The player's feet, floored to block coordinates.
fn player_block_pos(player: &pumpkin_plugin_api::player::Player) -> BlockPos {
    let p = player.get_position();
    BlockPos {
        x: p.0.floor() as i32,
        y: p.1.floor() as i32,
        z: p.2.floor() as i32,
    }
}

// (min corner, max corner, dimensions, volume)
type SelectionBounds = (BlockPos, BlockPos, (i32, i32, i32), u64);

// Look up the player's selection and reject it if it exceeds the volume limit.
fn selection_bounds(player_name: &str, max_volume: u64) -> Result<SelectionBounds, CommandError> {
    let sel = PLAYER_SELECTIONS.lock().unwrap();
    let map = sel
        .as_ref()
        .ok_or_else(|| CommandError::CommandFailed(msg_error("No selection set.")))?;
    let s = map.get(player_name).ok_or_else(|| {
        CommandError::CommandFailed(msg_error(
            "No selection set. Use wand or /schematic pos1/pos2.",
        ))
    })?;
    let volume = s.volume();
    if volume > max_volume {
        return Err(CommandError::CommandFailed(msg_error(&format!(
            "Selection too large ({volume} blocks, max {max_volume})."
        ))));
    }
    let (min, max) = s.bounds();
    Ok((min, max, s.dimensions(), volume))
}

// ── Load ────────────────────────────────────────────────────────────

pub(crate) struct LoadHandler {
    pub schematics_dir: String,
}

impl CommandHandler for LoadHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();

        let file_arg = simple_arg(&args, "file", "Expected file name")?;

        if !is_safe_filename(&file_arg) {
            sender.send_message(msg_error("Invalid file name."));
            return Ok(1);
        }

        let path = format!("{}/{}", self.schematics_dir, file_arg);
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                sender.send_message(msg_error(&format!("Failed to read file: {e}")));
                return Ok(1);
            }
        };

        let schematic = match parser::parse_schematic(&data, &file_arg) {
            Ok(s) => s,
            Err(e) => {
                debug_log(&format!("parse failed for '{file_arg}': {e}"));
                sender.send_message(msg_error(&format!("Parse error: {e}")));
                return Ok(1);
            }
        };

        let config = get_config();
        let fallback = resolve_fallback_block(&config);

        let palette_map: Vec<Vec<Option<u16>>> = schematic
            .regions
            .iter()
            .map(|r| resolve_palette(&r.palette, fallback))
            .collect();

        // Count blocks that don't natively resolve, independent of the fallback
        // (a valid fallback fills them in palette_map, hiding the count there).
        let mut unresolved_count = 0usize;
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
                    unresolved_count += 1;
                }
            }
        }

        let region_count = schematic.regions.len();
        let name = schematic.name.clone();

        let clip = match schematic_to_clipboard(name.clone(), &schematic, &palette_map) {
            Ok(c) => c,
            Err(e) => {
                sender.send_message(msg_error(&format!("Cannot load: {e}")));
                return Ok(1);
            }
        };
        let total_blocks = clip.blocks.iter().filter(|&&b| b != 0).count();

        if let Some(ref mut m) = *PLAYER_CLIPBOARDS.lock().unwrap() {
            m.insert(player_name, clip);
        }

        debug_log(&format!(
            "loaded '{file_arg}' as '{name}': {region_count} region(s), {total_blocks} block(s), {unresolved_count} unresolved"
        ));

        sender.send_message(msg_success(&format!(
            "Loaded '{name}' - {region_count} region(s), {total_blocks} blocks"
        )));

        if unresolved_count > 0 {
            if fallback.is_none() {
                sender.send_message(msg_warn(&format!(
                    "{unresolved_count} block type(s) skipped (unsupported). Use /schematic info for details."
                )));
            } else {
                sender.send_message(msg_warn(&format!(
                    "{unresolved_count} block type(s) replaced with {}. Use /schematic info for details.",
                    config.fallback_block
                )));
            }
        }

        Ok(0)
    }
}

// ── Paste ───────────────────────────────────────────────────────────

pub(crate) struct PasteHandler;

impl CommandHandler for PasteHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();
        let origin = player_block_pos(&player);
        let player_world = player.get_world();

        let config = get_config();
        let current = ACTIVE_PASTES.load(Ordering::Relaxed);
        if current >= config.max_concurrent_pastes {
            sender.send_message(msg_error("Server is busy. Please wait and try again."));
            return Ok(1);
        }

        let clips = PLAYER_CLIPBOARDS.lock().unwrap();
        let clip = clips
            .as_ref()
            .and_then(|m| m.get(&player_name))
            .ok_or_else(|| {
                CommandError::CommandFailed(msg_error(
                    "Nothing to paste. Use /schematic copy or /schematic load <file>",
                ))
            })?;

        let (work_queue, tile_entities) = clip.to_work_queue(origin);
        let total = work_queue.len();
        let name = clip.name.clone();
        drop(clips);

        let dimension = player_world.get_dimension();

        debug_log(&format!(
            "paste '{name}' by {player_name}: {total} block(s), {} tile entity/entities in '{dimension}' at ({}, {}, {}), {} block(s)/tick",
            tile_entities.len(),
            origin.x,
            origin.y,
            origin.z,
            config.blocks_per_tick
        ));

        sender.send_message(msg_info(&format!("Pasting '{name}' ({total} blocks)...")));

        schedule_paste(
            work_queue,
            tile_entities,
            dimension,
            config.blocks_per_tick,
            player_name,
            name,
            origin,
        );

        Ok(0)
    }
}

// ── List ────────────────────────────────────────────────────────────

pub(crate) struct ListHandler {
    pub schematics_dir: String,
}

impl CommandHandler for ListHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let entries = match std::fs::read_dir(&self.schematics_dir) {
            Ok(e) => e,
            Err(_) => {
                sender.send_message(msg_warn("No schematics found."));
                return Ok(0);
            }
        };

        let mut files: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "litematica" || ext == "litematic" || ext == "schem")
            })
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();

        files.sort();

        if files.is_empty() {
            sender.send_message(msg_warn("No schematics found."));
        } else {
            let header = msg_info(&format!("Available schematics ({}):", files.len()));
            sender.send_message(header);
            for file in &files {
                let entry = TextComponent::text(&format!("  - {file}"));
                entry.color_named(NamedColor::Gray);
                sender.send_message(entry);
            }
        }

        Ok(0)
    }
}

// ── Info ─────────────────────────────────────────────────────────────

pub(crate) struct InfoHandler;

impl CommandHandler for InfoHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();

        let clips = PLAYER_CLIPBOARDS.lock().unwrap();
        let clip = clips
            .as_ref()
            .and_then(|m| m.get(&player_name))
            .ok_or_else(|| CommandError::CommandFailed(msg_error("Nothing loaded or copied.")))?;

        let non_air = clip.blocks.iter().filter(|&&b| b != 0).count();

        sender.send_message(msg_info(&format!("Schematic: {}", clip.name)));
        let detail = TextComponent::text(&format!(
            "  {}x{}x{}, {} blocks, {} tile entities",
            clip.size_x,
            clip.size_y,
            clip.size_z,
            non_air,
            clip.tile_entities.len(),
        ));
        detail.color_named(NamedColor::Gray);
        sender.send_message(detail);

        Ok(0)
    }
}

// ── Status ──────────────────────────────────────────────────────────

pub(crate) struct StatusHandler;

impl CommandHandler for StatusHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let active = ACTIVE_PASTES.load(Ordering::Relaxed);
        if active == 0 {
            sender.send_message(msg_info("No active paste operations."));
        } else {
            sender.send_message(msg_warn(&format!(
                "{active} paste operation(s) in progress."
            )));
        }
        Ok(0)
    }
}

// ── Help ────────────────────────────────────────────────────────────

pub(crate) struct HelpHandler;

impl CommandHandler for HelpHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        sender.send_message(msg_info(&format!("Pumpkinetica v{PLUGIN_VERSION}")));

        let cmds = [
            ("/schem load <file>", "Load a schematic file"),
            ("/schem paste", "Paste clipboard or loaded schematic"),
            ("/schem list", "List available schematic files"),
            ("/schem info", "Show details of loaded schematic"),
            ("/schem status", "Show active operations"),
            ("/schem reload", "Reload config from disk"),
            ("/schem wand", "Get the selection wand"),
            ("/schem pos1/pos2", "Set selection at looked-at block"),
            ("/schem copy", "Copy selection to clipboard"),
            ("/schem save <name>", "Save clipboard to .schem file"),
            ("/schem rotate <90|180|270>", "Rotate clipboard"),
            ("/schem flip <x|z>", "Mirror clipboard"),
            ("/schem undo", "Undo last operation"),
            ("/schem redo", "Redo last undone operation"),
            ("/schem replace <from> <to>", "Replace blocks in selection"),
            ("/schem set <block>", "Fill selection with a block"),
        ];

        for (cmd, desc) in &cmds {
            let line = TextComponent::text(&format!("  {cmd}"));
            line.color_named(NamedColor::Green);
            let detail = TextComponent::text(&format!(" - {desc}"));
            detail.color_named(NamedColor::Gray);
            line.add_child(detail);
            sender.send_message(line);
        }

        Ok(0)
    }
}

// ── Reload ──────────────────────────────────────────────────────────

pub(crate) struct ReloadHandler {
    pub data_folder: String,
}

impl CommandHandler for ReloadHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let config = crate::load_config(&self.data_folder);
        let reload_msg = format!(
            "Config reloaded. Wand: {}, blocks/tick: {}, max pastes: {}",
            config.wand_item, config.blocks_per_tick, config.max_concurrent_pastes
        );
        *crate::CONFIG.lock().unwrap() = Some(config);
        debug_log("config reloaded");
        sender.send_message(msg_success(&reload_msg));
        Ok(0)
    }
}

// ── Pos1 / Pos2 ─────────────────────────────────────────────────────

// Set pos1 or pos2 at the looked-at block, falling back to the player's feet.
fn set_selection_pos(sender: &CommandSender, is_pos1: bool) -> Result<i32, CommandError> {
    let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
    let player_name = player.get_name();
    let entity = player.as_entity();

    let pos = match entity.raycast(5.0, false) {
        Some(hit) => hit.pos,
        None => player_block_pos(&player),
    };

    let mut sel = PLAYER_SELECTIONS.lock().unwrap();
    if let Some(ref mut map) = *sel {
        let entry = map.entry(player_name).or_insert(Selection {
            pos1: pos,
            pos2: pos,
        });
        if is_pos1 {
            entry.pos1 = pos;
        } else {
            entry.pos2 = pos;
        }
    }

    let label = if is_pos1 { "Pos1" } else { "Pos2" };
    sender.send_message(msg_info(&format!(
        "{label} set to ({}, {}, {})",
        pos.x, pos.y, pos.z
    )));
    Ok(0)
}

pub(crate) struct Pos1Handler;

impl CommandHandler for Pos1Handler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        set_selection_pos(&sender, true)
    }
}

pub(crate) struct Pos2Handler;

impl CommandHandler for Pos2Handler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        set_selection_pos(&sender, false)
    }
}

// ── Wand ────────────────────────────────────────────────────────────

pub(crate) struct WandHandler;

impl CommandHandler for WandHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let config = get_config();
        let item = ItemStack::new(&config.wand_item, 1);
        player.set_item_in_hand(Hand::Right, Some(item));
        sender.send_message(msg_success(&format!(
            "Gave selection wand ({}). Left-click: pos1, Right-click: pos2",
            config.wand_item
        )));
        Ok(0)
    }
}

// ── Copy ────────────────────────────────────────────────────────────

pub(crate) struct CopyHandler;

impl CommandHandler for CopyHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();
        let config = get_config();

        let (min, max, (sx, sy, sz), _volume) =
            selection_bounds(&player_name, config.max_selection_volume)?;

        let world = player.get_world();

        let (blocks, tile_entities) = read_selection_chunk_batched(&world, min, max, sx, sy, sz);

        let non_air = blocks.iter().filter(|&&b| b != 0).count();

        let clip = Clipboard {
            name: "clipboard".into(),
            blocks,
            tile_entities,
            size_x: sx,
            size_y: sy,
            size_z: sz,
        };

        {
            let mut clips = PLAYER_CLIPBOARDS.lock().unwrap();
            if let Some(ref mut map) = *clips {
                map.insert(player_name, clip);
            }
        }

        sender.send_message(msg_success(&format!(
            "Copied {non_air} blocks ({sx}x{sy}x{sz})"
        )));
        Ok(0)
    }
}

// ── Save ────────────────────────────────────────────────────────────

pub(crate) struct SaveHandler {
    pub schematics_dir: String,
}

impl CommandHandler for SaveHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();

        let name = simple_arg(&args, "name", "Expected schematic name")?;

        if !is_safe_filename(&name) {
            sender.send_message(msg_error("Invalid schematic name."));
            return Ok(1);
        }

        let clips = PLAYER_CLIPBOARDS.lock().unwrap();
        let clip = clips
            .as_ref()
            .and_then(|m| m.get(&player_name))
            .ok_or_else(|| {
                CommandError::CommandFailed(msg_error("No clipboard. Use /schematic copy first."))
            })?;

        let (palette_strings, indices, unresolved) = clipboard_to_schem_data(clip);

        let tile_entities: Vec<(i32, i32, i32, Vec<u8>)> = clip
            .tile_entities
            .iter()
            .map(|(p, nbt)| (p.x, p.y, p.z, nbt.clone()))
            .collect();

        let nbt = parser::build_schem_nbt(
            &palette_strings,
            &indices,
            (clip.size_x, clip.size_y, clip.size_z),
            &tile_entities,
        );

        drop(clips);

        let bytes = match parser::write_schem_bytes(&nbt) {
            Ok(b) => b,
            Err(e) => {
                sender.send_message(msg_error(&format!("Failed to encode schematic: {e}")));
                return Ok(1);
            }
        };

        let filename = if name.ends_with(".schem") {
            name.to_string()
        } else {
            format!("{name}.schem")
        };
        let path = format!("{}/{filename}", self.schematics_dir);

        match std::fs::write(&path, &bytes) {
            Ok(()) => {
                sender.send_message(msg_success(&format!(
                    "Saved '{filename}' ({} bytes)",
                    bytes.len()
                )));
                if unresolved > 0 {
                    sender.send_message(msg_warn(&format!(
                        "{unresolved} block type(s) could not be identified and were saved as stone."
                    )));
                }
            }
            Err(e) => {
                sender.send_message(msg_error(&format!("Failed to write file: {e}")));
            }
        }

        Ok(0)
    }
}

// ── Rotate ──────────────────────────────────────────────────────────

pub(crate) struct RotateHandler;

impl CommandHandler for RotateHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();

        let degrees_str = simple_arg(&args, "degrees", "Expected degrees (90, 180, 270)")?;

        let degrees: i32 = match degrees_str.parse() {
            Ok(d) if d == 90 || d == 180 || d == 270 => d,
            _ => {
                sender.send_message(msg_error("Degrees must be 90, 180, or 270."));
                return Ok(1);
            }
        };

        let mut clips = PLAYER_CLIPBOARDS.lock().unwrap();
        let clip = clips
            .as_mut()
            .and_then(|m| m.get_mut(&player_name))
            .ok_or_else(|| {
                CommandError::CommandFailed(msg_error("No clipboard. Use /schematic copy first."))
            })?;

        rotate_clipboard(clip, degrees);

        sender.send_message(msg_success(&format!(
            "Rotated {degrees}°. Size: {}x{}x{}",
            clip.size_x, clip.size_y, clip.size_z
        )));
        Ok(0)
    }
}

// ── Flip ────────────────────────────────────────────────────────────

pub(crate) struct FlipHandler;

impl CommandHandler for FlipHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();

        let axis_str = simple_arg(&args, "axis", "Expected axis (x or z)")?;

        let axis = match axis_str.as_str() {
            "x" | "X" => FlipAxis::X,
            "z" | "Z" => FlipAxis::Z,
            _ => {
                sender.send_message(msg_error("Axis must be x or z."));
                return Ok(1);
            }
        };

        let mut clips = PLAYER_CLIPBOARDS.lock().unwrap();
        let clip = clips
            .as_mut()
            .and_then(|m| m.get_mut(&player_name))
            .ok_or_else(|| {
                CommandError::CommandFailed(msg_error("No clipboard. Use /schematic copy first."))
            })?;

        flip_clipboard(clip, axis);

        sender.send_message(msg_success(&format!("Flipped along {axis_str} axis.")));
        Ok(0)
    }
}

// ── Undo ────────────────────────────────────────────────────────────

// Undo pops the last op and restores its old states; redo mirrors it with the
// new states. The popped entry moves to the opposite stack.
fn run_history_op(sender: &CommandSender, is_undo: bool) -> Result<i32, CommandError> {
    let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
    let player_name = player.get_name();
    let config = get_config();

    let noun = if is_undo { "undo" } else { "redo" };
    let entry = {
        let mut histories = PLAYER_HISTORIES.lock().unwrap();
        let history = histories
            .as_mut()
            .and_then(|m| m.get_mut(&player_name))
            .ok_or_else(|| {
                CommandError::CommandFailed(msg_error(&format!("Nothing to {noun}.")))
            })?;
        let stack = if is_undo {
            &mut history.undo_stack
        } else {
            &mut history.redo_stack
        };
        let Some(entry) = stack.pop_back() else {
            return Err(CommandError::CommandFailed(msg_error(&format!(
                "Nothing to {noun}."
            ))));
        };
        entry
    };

    let states = if is_undo {
        &entry.old_states
    } else {
        &entry.new_states
    };
    let block_count = states.len();
    let desc = entry.description.clone();
    let dimension = entry.dimension.clone();
    let work_queue: Vec<BlockPlacement> = states
        .iter()
        .map(|s| BlockPlacement {
            pos: s.pos,
            state_id: s.state_id,
        })
        .collect();

    {
        let mut histories = PLAYER_HISTORIES.lock().unwrap();
        if let Some(ref mut map) = *histories {
            let history = map
                .entry(player_name.clone())
                .or_insert_with(PlayerHistory::new);
            if is_undo {
                history.redo_stack.push_back(entry);
            } else {
                history.undo_stack.push_back(entry);
            }
        }
    }

    let (verb, prefix) = if is_undo {
        ("Undoing", "Undo")
    } else {
        ("Redoing", "Redo")
    };
    debug_log(&format!(
        "{noun} by {player_name}: {block_count} block(s) in '{dimension}' ({desc})"
    ));
    sender.send_message(msg_info(&format!(
        "{verb}: {desc} ({block_count} blocks)..."
    )));

    schedule_block_op(
        work_queue,
        vec![],
        dimension,
        config.blocks_per_tick,
        player_name,
        format!("{prefix}: {desc}"),
        false,
    );

    Ok(0)
}

pub(crate) struct UndoHandler;

impl CommandHandler for UndoHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        run_history_op(&sender, true)
    }
}

// ── Redo ────────────────────────────────────────────────────────────

pub(crate) struct RedoHandler;

impl CommandHandler for RedoHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        run_history_op(&sender, false)
    }
}

// ── Replace ─────────────────────────────────────────────────────────

pub(crate) struct ReplaceHandler;

impl CommandHandler for ReplaceHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();
        let config = get_config();

        let from_str = simple_arg(&args, "from", "Expected source block")?;
        let to_str = simple_arg(&args, "to", "Expected target block")?;

        let from_id = world::resolve_block_state(&from_str, &[]).ok_or_else(|| {
            CommandError::CommandFailed(msg_error(&format!("Unknown block: {from_str}")))
        })?;
        let to_id = world::resolve_block_state(&to_str, &[]).ok_or_else(|| {
            CommandError::CommandFailed(msg_error(&format!("Unknown block: {to_str}")))
        })?;

        let (min, max, _dims, _volume) =
            selection_bounds(&player_name, config.max_selection_volume)?;

        let world = player.get_world();
        let dimension = world.get_dimension();

        let mut work_queue = Vec::new();
        let min_cx = min.x.div_euclid(16);
        let max_cx = max.x.div_euclid(16);
        let min_cz = min.z.div_euclid(16);
        let max_cz = max.z.div_euclid(16);

        for cx in min_cx..=max_cx {
            for cz in min_cz..=max_cz {
                let chunk = world.get_chunk(cx, cz);
                let chunk_min_x = (cx * 16).max(min.x);
                let chunk_max_x = (cx * 16 + 15).min(max.x);
                let chunk_min_z = (cz * 16).max(min.z);
                let chunk_max_z = (cz * 16 + 15).min(max.z);

                for y in min.y..=max.y {
                    for z in chunk_min_z..=chunk_max_z {
                        for x in chunk_min_x..=chunk_max_x {
                            let pos = BlockPos { x, y, z };
                            let state_id = match &chunk {
                                Some(c) => {
                                    let local = BlockPos {
                                        x: x.rem_euclid(16),
                                        y,
                                        z: z.rem_euclid(16),
                                    };
                                    c.get_block_state_id(local)
                                }
                                None => world.get_block_state_id(pos),
                            };
                            if state_id == from_id {
                                work_queue.push(BlockPlacement {
                                    pos,
                                    state_id: to_id,
                                });
                            }
                        }
                    }
                }
            }
        }

        if work_queue.is_empty() {
            sender.send_message(msg_warn("No matching blocks found."));
            return Ok(0);
        }

        let count = work_queue.len();
        sender.send_message(msg_info(&format!(
            "Replacing {count} blocks: {from_str} -> {to_str}..."
        )));

        schedule_block_op(
            work_queue,
            vec![],
            dimension,
            config.blocks_per_tick,
            player_name,
            format!("Replace {from_str} -> {to_str}"),
            true,
        );

        Ok(0)
    }
}

// ── Set ─────────────────────────────────────────────────────────────

pub(crate) struct SetHandler;

impl CommandHandler for SetHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();
        let config = get_config();

        let block_str = simple_arg(&args, "block", "Expected block name")?;

        let state_id = world::resolve_block_state(&block_str, &[]).ok_or_else(|| {
            CommandError::CommandFailed(msg_error(&format!("Unknown block: {block_str}")))
        })?;

        let (min, max, _dims, volume) =
            selection_bounds(&player_name, config.max_selection_volume)?;

        let mut work_queue = Vec::with_capacity(volume as usize);
        for y in min.y..=max.y {
            for z in min.z..=max.z {
                for x in min.x..=max.x {
                    work_queue.push(BlockPlacement {
                        pos: BlockPos { x, y, z },
                        state_id,
                    });
                }
            }
        }

        let count = work_queue.len();
        let world = player.get_world();
        let dimension = world.get_dimension();

        sender.send_message(msg_info(&format!(
            "Setting {count} blocks to {block_str}..."
        )));

        schedule_block_op(
            work_queue,
            vec![],
            dimension,
            config.blocks_per_tick,
            player_name,
            format!("Set {block_str}"),
            true,
        );

        Ok(0)
    }
}
