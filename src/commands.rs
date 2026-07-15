use std::sync::atomic::Ordering;

use pumpkin_plugin_api::commands::CommandHandler;
use pumpkin_plugin_api::{
    Result, Server,
    command::{CommandError, CommandSender, ConsumedArgs},
    command_wit::Arg,
    common::{BlockPos, NamedColor},
    text::TextComponent,
};

use crate::parser;
use crate::paste::{BlockPlacement, TileEntityPlacement, schedule_paste};
use crate::{
    ACTIVE_PASTES, LOADED_SCHEMATICS, LoadedSchematic, PLUGIN_VERSION, get_config, msg_error,
    msg_info, msg_success, msg_warn, resolve_fallback_block, resolve_palette,
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
        let pos = player.get_position();
        let origin = BlockPos {
            x: pos.0 as i32,
            y: pos.1 as i32,
            z: pos.2 as i32,
        };
        let player_world = player.get_world();

        let config = get_config();
        let current = ACTIVE_PASTES.load(Ordering::Relaxed);
        if current >= config.max_concurrent_pastes {
            sender.send_message(msg_error("Server is busy. Please wait and try again."));
            return Ok(1);
        }

        let map = LOADED_SCHEMATICS.lock().unwrap();
        let loaded = map
            .as_ref()
            .and_then(|m| m.get(&player_name))
            .ok_or_else(|| {
                CommandError::CommandFailed(msg_error(
                    "No schematic loaded. Use /schematic load <file>",
                ))
            })?;

        let mut work_queue: Vec<BlockPlacement> = Vec::new();
        let mut tile_entities: Vec<TileEntityPlacement> = Vec::new();

        for (region_idx, region) in loaded.schematic.regions.iter().enumerate() {
            let [sx, sy, sz] = region.abs_size();
            let palette_map = &loaded.palette_map[region_idx];

            let offset_x = if region.size[0] < 0 {
                region.position[0] + region.size[0] + 1
            } else {
                region.position[0]
            };
            let offset_y = if region.size[1] < 0 {
                region.position[1] + region.size[1] + 1
            } else {
                region.position[1]
            };
            let offset_z = if region.size[2] < 0 {
                region.position[2] + region.size[2] + 1
            } else {
                region.position[2]
            };

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
            ("/schematic load <file>", "Load a schematic file"),
            (
                "/schematic paste",
                "Paste loaded schematic at your position",
            ),
            ("/schematic list", "List available schematic files"),
            ("/schematic info", "Show details of loaded schematic"),
            ("/schematic status", "Show active paste operations"),
            ("/schematic reload", "Reload config from disk"),
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
        *crate::CONFIG.lock().unwrap() = Some(config.clone());
        sender.send_message(msg_success(&format!(
            "Config reloaded. Fallback: {}, blocks/tick: {}, max pastes: {}",
            config.fallback_block, config.blocks_per_tick, config.max_concurrent_pastes
        )));
        Ok(0)
    }
}
