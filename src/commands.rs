use std::sync::atomic::Ordering;

use pumpkin_plugin_api::commands::CommandHandler;
use pumpkin_plugin_api::{
    Result, Server,
    command::{CommandError, CommandSender, ConsumedArgs},
    command_wit::Arg,
    common::{BlockPos, Hand, NamedColor},
    player::ItemStack,
    text::TextComponent,
};

use crate::clipboard::{
    Clipboard, FlipAxis, clipboard_to_schem_data, flip_clipboard, read_selection_chunk_batched,
    rotate_clipboard,
};
use crate::history::PlayerHistory;
use crate::parser;
use crate::paste::{BlockPlacement, TileEntityPlacement, schedule_block_op, schedule_paste};
use crate::selection::Selection;
use crate::{
    ACTIVE_PASTES, LOADED_SCHEMATICS, PLAYER_CLIPBOARDS, PLAYER_HISTORIES, PLAYER_SELECTIONS,
    PLUGIN_VERSION, REVERSE_REGISTRY, LoadedSchematic, get_config, msg_error, msg_info,
    msg_success, msg_warn, resolve_and_register, resolve_fallback_block, resolve_palette,
};

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

        let file_arg = match args.get_value("file") {
            Arg::Simple(s) => s,
            _ => {
                return Err(CommandError::InvalidConsumption(Some(
                    "Expected file name".into(),
                )));
            }
        };

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

        let total_blocks: usize = schematic
            .regions
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let [sx, sy, sz] = r.abs_size();
                let pm = &palette_map[i];
                let mut count = 0usize;
                for y in 0..sy {
                    for z in 0..sz {
                        for x in 0..sx {
                            let idx = r.get_palette_index(x, y, z) as usize;
                            if idx < pm.len()
                                && let Some(state_id) = pm[idx]
                                && state_id != 0
                            {
                                count += 1;
                            }
                        }
                    }
                }
                count
            })
            .sum();

        let mut unresolved_count = 0usize;
        for (i, pm) in palette_map.iter().enumerate() {
            for (j, entry) in pm.iter().enumerate() {
                if entry.is_none() && schematic.regions[i].palette[j].name != "minecraft:air" {
                    unresolved_count += 1;
                }
            }
        }

        let region_count = schematic.regions.len();
        let name = schematic.name.clone();

        {
            let mut map = LOADED_SCHEMATICS.lock().unwrap();
            if let Some(ref mut m) = *map {
                m.insert(
                    player_name,
                    LoadedSchematic {
                        schematic,
                        palette_map,
                    },
                );
            }
        }

        sender.send_message(msg_success(&format!(
            "Loaded '{name}' - {region_count} region(s), {total_blocks} blocks"
        )));

        if unresolved_count > 0 && config.fallback_block == "skip" {
            sender.send_message(msg_warn(&format!(
                "{unresolved_count} block type(s) skipped (unsupported). Use /schematic info for details."
            )));
        } else if unresolved_count > 0 {
            sender.send_message(msg_warn(&format!(
                "{unresolved_count} block type(s) replaced with {}. Use /schematic info for details.",
                config.fallback_block
            )));
        }

        Ok(0)
    }
}

// ── Paste ───────────────────────────────────────────────────────────

pub(crate) struct PasteHandler {
    /// When true, ignore the clipboard/schematic offset and anchor the
    /// structure's minimum corner at the player's feet. `/schematic paste here`.
    pub(crate) at_feet: bool,
}

impl CommandHandler for PasteHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();
        let pos = player.get_position();
        let origin = BlockPos {
            x: pos.0.floor() as i32,
            y: pos.1.floor() as i32,
            z: pos.2.floor() as i32,
        };
        let player_world = player.get_world();

        let config = get_config();
        let current = ACTIVE_PASTES.load(Ordering::Relaxed);
        if current >= config.max_concurrent_pastes {
            sender.send_message(msg_error("Server is busy. Please wait and try again."));
            return Ok(1);
        }

        // Try clipboard first, then loaded schematic
        {
            let clips = PLAYER_CLIPBOARDS.lock().unwrap();
            if let Some(ref map) = *clips
                && let Some(clip) = map.get(&player_name)
            {
                let (work_queue, tile_entities) = clip.to_work_queue(origin, self.at_feet);
                let total = work_queue.len();
                let dimension = player_world.get_dimension();

                sender.send_message(msg_info(&format!("Pasting clipboard ({total} blocks)...")));

                schedule_paste(
                    work_queue,
                    tile_entities,
                    dimension,
                    config.blocks_per_tick,
                    player_name,
                    "clipboard".into(),
                    origin,
                );
                return Ok(0);
            }
        }

        let map = LOADED_SCHEMATICS.lock().unwrap();
        let loaded = map
            .as_ref()
            .and_then(|m| m.get(&player_name))
            .ok_or_else(|| {
                CommandError::CommandFailed(msg_error(
                    "No clipboard or schematic loaded. Use /schematic copy or /schematic load <file>",
                ))
            })?;

        let mut work_queue: Vec<BlockPlacement> = Vec::new();
        let mut tile_entities: Vec<TileEntityPlacement> = Vec::new();

        // Normalized min corner of a region in paste space (folds in the
        // negative-size flip so mirrored regions anchor at their true min).
        let region_min = |region: &crate::parser::Region| {
            let n = |pos: i32, size: i32| if size < 0 { pos + size + 1 } else { pos };
            [
                n(region.position[0], region.size[0]),
                n(region.position[1], region.size[1]),
                n(region.position[2], region.size[2]),
            ]
        };

        // In `at_feet` mode, shift the whole schematic so its global minimum
        // corner lands on the player — preserving inter-region layout.
        let shift = if self.at_feet {
            loaded.schematic.regions.iter().fold(
                [i32::MAX, i32::MAX, i32::MAX],
                |acc, region| {
                    let m = region_min(region);
                    [acc[0].min(m[0]), acc[1].min(m[1]), acc[2].min(m[2])]
                },
            )
        } else {
            [0, 0, 0]
        };

        for (region_idx, region) in loaded.schematic.regions.iter().enumerate() {
            let [sx, sy, sz] = region.abs_size();
            let palette_map = &loaded.palette_map[region_idx];

            let m = region_min(region);
            let offset_x = m[0] - shift[0];
            let offset_y = m[1] - shift[1];
            let offset_z = m[2] - shift[2];

            for y in 0..sy {
                for z in 0..sz {
                    for x in 0..sx {
                        let palette_idx = region.get_palette_index(x, y, z) as usize;

                        if palette_idx >= palette_map.len() {
                            continue;
                        }

                        let Some(state_id) = palette_map[palette_idx] else {
                            continue;
                        };

                        if state_id == 0 {
                            continue;
                        }

                        work_queue.push(BlockPlacement {
                            pos: BlockPos {
                                x: origin.x + offset_x + x,
                                y: origin.y + offset_y + y,
                                z: origin.z + offset_z + z,
                            },
                            state_id,
                        });
                    }
                }
            }

            for te in &region.tile_entities {
                tile_entities.push(TileEntityPlacement {
                    pos: BlockPos {
                        x: origin.x + offset_x + te.x,
                        y: origin.y + offset_y + te.y,
                        z: origin.z + offset_z + te.z,
                    },
                    nbt: te.raw_nbt.clone(),
                });
            }
        }

        let total = work_queue.len();
        let schematic_name = loaded.schematic.name.clone();
        drop(map);

        let dimension = player_world.get_dimension();

        sender.send_message(msg_info(&format!(
            "Pasting '{schematic_name}' ({total} blocks)..."
        )));

        schedule_paste(
            work_queue,
            tile_entities,
            dimension,
            config.blocks_per_tick,
            player_name,
            schematic_name,
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

        let map = LOADED_SCHEMATICS.lock().unwrap();
        let loaded = map
            .as_ref()
            .and_then(|m| m.get(&player_name))
            .ok_or_else(|| CommandError::CommandFailed(msg_error("No schematic loaded.")))?;

        let schematic = &loaded.schematic;

        let header = msg_info(&format!("Schematic: {}", schematic.name));
        sender.send_message(header);

        for (i, region) in schematic.regions.iter().enumerate() {
            let [sx, sy, sz] = region.abs_size();
            let region_msg = TextComponent::text(&format!(
                "  {} - {}x{}x{}, {} blocks, {} tile entities",
                region.name,
                sx,
                sy,
                sz,
                region.palette.len(),
                region.tile_entities.len(),
            ));
            region_msg.color_named(NamedColor::Gray);
            sender.send_message(region_msg);

            let unresolved: Vec<&str> = loaded.palette_map[i]
                .iter()
                .zip(region.palette.iter())
                .filter(|(state, entry)| {
                    state.is_none() && entry.name != "minecraft:air" && entry.name != "air"
                })
                .map(|(_, entry)| entry.name.as_str())
                .collect();

            if !unresolved.is_empty() {
                let warn =
                    TextComponent::text(&format!("    Unsupported: {}", unresolved.join(", ")));
                warn.color_named(NamedColor::Yellow);
                sender.send_message(warn);
            }
        }

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
        sender.send_message(msg_info(&format!("PSchematics v{PLUGIN_VERSION}")));

        let cmds = [
            ("/schem load <file>", "Load a schematic file"),
            ("/schem paste", "Paste clipboard or loaded schematic"),
            ("/schem paste here", "Paste with min corner at your feet"),
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
        sender.send_message(msg_success(&reload_msg));
        Ok(0)
    }
}

// ── Pos1 / Pos2 ─────────────────────────────────────────────────────

pub(crate) struct Pos1Handler;

impl CommandHandler for Pos1Handler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();
        let entity = player.as_entity();

        let pos = if let Some(hit) = entity.raycast(5.0, false) {
            hit.pos
        } else {
            let p = player.get_position();
            BlockPos {
                x: p.0.floor() as i32,
                y: p.1.floor() as i32,
                z: p.2.floor() as i32,
            }
        };

        let mut sel = PLAYER_SELECTIONS.lock().unwrap();
        if let Some(ref mut map) = *sel {
            let entry = map.entry(player_name).or_insert(Selection {
                pos1: pos,
                pos2: pos,
            });
            entry.pos1 = pos;
        }

        sender.send_message(msg_info(&format!(
            "Pos1 set to ({}, {}, {})",
            pos.x, pos.y, pos.z
        )));
        Ok(0)
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
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();
        let entity = player.as_entity();

        let pos = if let Some(hit) = entity.raycast(5.0, false) {
            hit.pos
        } else {
            let p = player.get_position();
            BlockPos {
                x: p.0.floor() as i32,
                y: p.1.floor() as i32,
                z: p.2.floor() as i32,
            }
        };

        let mut sel = PLAYER_SELECTIONS.lock().unwrap();
        if let Some(ref mut map) = *sel {
            let entry = map.entry(player_name).or_insert(Selection {
                pos1: pos,
                pos2: pos,
            });
            entry.pos2 = pos;
        }

        sender.send_message(msg_info(&format!(
            "Pos2 set to ({}, {}, {})",
            pos.x, pos.y, pos.z
        )));
        Ok(0)
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

        let ((min, max), (sx, sy, sz), volume) = {
            let sel = PLAYER_SELECTIONS.lock().unwrap();
            let map = sel
                .as_ref()
                .ok_or_else(|| CommandError::CommandFailed(msg_error("No selection set.")))?;
            let s = map
                .get(&player_name)
                .ok_or_else(|| CommandError::CommandFailed(msg_error("No selection set. Use wand or /schematic pos1/pos2.")))?;
            (s.bounds(), s.dimensions(), s.volume())
        };

        if volume > config.max_selection_volume {
            sender.send_message(msg_error(&format!(
                "Selection too large ({volume} blocks, max {}).",
                config.max_selection_volume
            )));
            return Ok(1);
        }

        let player_pos = player.get_position();
        let player_block = BlockPos {
            x: player_pos.0.floor() as i32,
            y: player_pos.1.floor() as i32,
            z: player_pos.2.floor() as i32,
        };

        let world = player.get_world();

        let (blocks, tile_entities) = read_selection_chunk_batched(&world, min, max, sx, sy, sz);

        let non_air = blocks.iter().filter(|&&b| b != 0).count();

        let clip = Clipboard {
            blocks,
            tile_entities,
            size_x: sx,
            size_y: sy,
            size_z: sz,
            offset: BlockPos {
                x: player_block.x - min.x,
                y: player_block.y - min.y,
                z: player_block.z - min.z,
            },
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

        let name = match args.get_value("name") {
            Arg::Simple(s) => s,
            _ => {
                return Err(CommandError::InvalidConsumption(Some(
                    "Expected schematic name".into(),
                )));
            }
        };

        let clips = PLAYER_CLIPBOARDS.lock().unwrap();
        let clip = clips
            .as_ref()
            .and_then(|m| m.get(&player_name))
            .ok_or_else(|| {
                CommandError::CommandFailed(msg_error(
                    "No clipboard. Use /schematic copy first.",
                ))
            })?;

        let reg = REVERSE_REGISTRY.lock().unwrap();
        let registry = reg.as_ref().ok_or_else(|| {
            CommandError::CommandFailed(msg_error("Block registry not initialized."))
        })?;

        let (palette_strings, indices, unresolved) =
            clipboard_to_schem_data(clip, registry);

        let nbt = parser::build_schem_nbt(
            &palette_strings,
            &indices,
            (clip.size_x, clip.size_y, clip.size_z),
        );

        drop(reg);
        drop(clips);

        let bytes = parser::write_schem_bytes(&nbt);

        let filename = if name.ends_with(".schem") {
            name.to_string()
        } else {
            format!("{name}.schem")
        };
        let path = format!("{}/{filename}", self.schematics_dir);

        match std::fs::write(&path, &bytes) {
            Ok(()) => {
                sender.send_message(msg_success(&format!("Saved '{filename}' ({} bytes)", bytes.len())));
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

        let degrees_str = match args.get_value("degrees") {
            Arg::Simple(s) => s,
            _ => {
                return Err(CommandError::InvalidConsumption(Some(
                    "Expected degrees (90, 180, 270)".into(),
                )));
            }
        };

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

        let axis_str = match args.get_value("axis") {
            Arg::Simple(s) => s,
            _ => {
                return Err(CommandError::InvalidConsumption(Some(
                    "Expected axis (x or z)".into(),
                )));
            }
        };

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

pub(crate) struct UndoHandler;

impl CommandHandler for UndoHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();
        let config = get_config();

        let entry = {
            let mut histories = PLAYER_HISTORIES.lock().unwrap();
            let history = histories
                .as_mut()
                .and_then(|m| m.get_mut(&player_name))
                .ok_or_else(|| CommandError::CommandFailed(msg_error("Nothing to undo.")))?;

            let Some(entry) = history.undo_stack.pop_back() else {
                return Err(CommandError::CommandFailed(msg_error("Nothing to undo.")));
            };
            entry
        };

        let block_count = entry.old_states.len();
        let desc = entry.description.clone();
        let dimension = entry.dimension.clone();

        // Build work queue from old states (restore)
        let work_queue: Vec<BlockPlacement> = entry
            .old_states
            .iter()
            .map(|s| BlockPlacement {
                pos: s.pos,
                state_id: s.state_id,
            })
            .collect();

        // Push to redo before we move the entry
        {
            let mut histories = PLAYER_HISTORIES.lock().unwrap();
            if let Some(ref mut map) = *histories {
                let history = map
                    .entry(player_name.clone())
                    .or_insert_with(PlayerHistory::new);
                history.redo_stack.push_back(entry);
            }
        }

        sender.send_message(msg_info(&format!(
            "Undoing: {desc} ({block_count} blocks)..."
        )));

        schedule_block_op(
            work_queue,
            vec![],
            dimension,
            config.blocks_per_tick,
            player_name,
            format!("Undo: {desc}"),
            false,
        );

        Ok(0)
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
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();
        let config = get_config();

        let entry = {
            let mut histories = PLAYER_HISTORIES.lock().unwrap();
            let history = histories
                .as_mut()
                .and_then(|m| m.get_mut(&player_name))
                .ok_or_else(|| CommandError::CommandFailed(msg_error("Nothing to redo.")))?;

            let Some(entry) = history.redo_stack.pop_back() else {
                return Err(CommandError::CommandFailed(msg_error("Nothing to redo.")));
            };
            entry
        };

        let block_count = entry.new_states.len();
        let desc = entry.description.clone();
        let dimension = entry.dimension.clone();

        let work_queue: Vec<BlockPlacement> = entry
            .new_states
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
                history.undo_stack.push_back(entry);
            }
        }

        sender.send_message(msg_info(&format!(
            "Redoing: {desc} ({block_count} blocks)..."
        )));

        schedule_block_op(
            work_queue,
            vec![],
            dimension,
            config.blocks_per_tick,
            player_name,
            format!("Redo: {desc}"),
            false,
        );

        Ok(0)
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

        let from_str = match args.get_value("from") {
            Arg::Simple(s) => s,
            _ => {
                return Err(CommandError::InvalidConsumption(Some(
                    "Expected source block".into(),
                )));
            }
        };
        let to_str = match args.get_value("to") {
            Arg::Simple(s) => s,
            _ => {
                return Err(CommandError::InvalidConsumption(Some(
                    "Expected target block".into(),
                )));
            }
        };

        let from_id = resolve_and_register(&from_str, &[]).ok_or_else(|| {
            CommandError::CommandFailed(msg_error(&format!("Unknown block: {from_str}")))
        })?;
        let to_id = resolve_and_register(&to_str, &[]).ok_or_else(|| {
            CommandError::CommandFailed(msg_error(&format!("Unknown block: {to_str}")))
        })?;

        let (min, max, volume) = {
            let sel = PLAYER_SELECTIONS.lock().unwrap();
            let map = sel
                .as_ref()
                .ok_or_else(|| CommandError::CommandFailed(msg_error("No selection set.")))?;
            let s = map.get(&player_name).ok_or_else(|| {
                CommandError::CommandFailed(msg_error("No selection set."))
            })?;
            let (min, max) = s.bounds();
            (min, max, s.volume())
        };

        if volume > config.max_selection_volume {
            sender.send_message(msg_error(&format!(
                "Selection too large ({volume} blocks)."
            )));
            return Ok(1);
        }

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

        let block_str = match args.get_value("block") {
            Arg::Simple(s) => s,
            _ => {
                return Err(CommandError::InvalidConsumption(Some(
                    "Expected block name".into(),
                )));
            }
        };

        let state_id = resolve_and_register(&block_str, &[]).ok_or_else(|| {
            CommandError::CommandFailed(msg_error(&format!("Unknown block: {block_str}")))
        })?;

        let (min, max, volume) = {
            let sel = PLAYER_SELECTIONS.lock().unwrap();
            let map = sel
                .as_ref()
                .ok_or_else(|| CommandError::CommandFailed(msg_error("No selection set.")))?;
            let s = map.get(&player_name).ok_or_else(|| {
                CommandError::CommandFailed(msg_error("No selection set."))
            })?;
            let (min, max) = s.bounds();
            (min, max, s.volume())
        };

        if volume > config.max_selection_volume {
            sender.send_message(msg_error(&format!(
                "Selection too large ({volume} blocks)."
            )));
            return Ok(1);
        }

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
