use std::sync::Mutex;
use std::sync::atomic::Ordering;

use pumpkin_plugin_api::common::BlockPos;
use pumpkin_plugin_api::world;

use crate::history::{BlockSnapshot, PlayerHistory, UndoEntry};
use crate::{ACTIVE_PASTES, PLAYER_HISTORIES, get_config, msg_success};

pub(crate) struct BlockPlacement {
    pub pos: BlockPos,
    pub state_id: u16,
}

pub(crate) struct TileEntityPlacement {
    pub pos: BlockPos,
    pub nbt: Vec<u8>,
}

struct PasteState {
    blocks: Vec<(BlockPos, u16)>,
    block_idx: usize,
    tile_entities: Vec<TileEntityPlacement>,
    te_idx: usize,
    record_undo: bool,
    old_snapshots: Vec<BlockSnapshot>,
    new_snapshots: Vec<BlockSnapshot>,
}

pub(crate) fn schedule_paste(
    queue: Vec<BlockPlacement>,
    tile_entities: Vec<TileEntityPlacement>,
    dimension: String,
    blocks_per_tick: usize,
    player_name: String,
    schematic_name: String,
    origin: BlockPos,
) {
    schedule_block_op(
        queue,
        tile_entities,
        dimension,
        blocks_per_tick,
        player_name,
        format!(
            "Paste '{schematic_name}' at ({}, {}, {})",
            origin.x, origin.y, origin.z
        ),
        true,
    );
}

pub(crate) fn schedule_block_op(
    queue: Vec<BlockPlacement>,
    tile_entities: Vec<TileEntityPlacement>,
    dimension: String,
    blocks_per_tick: usize,
    player_name: String,
    description: String,
    record_undo: bool,
) {
    ACTIVE_PASTES.fetch_add(1, Ordering::Relaxed);

    let total_blocks: usize = queue.len();
    let blocks: Vec<(BlockPos, u16)> = queue.into_iter().map(|p| (p.pos, p.state_id)).collect();
    let state = std::sync::Arc::new(Mutex::new(PasteState {
        blocks,
        block_idx: 0,
        tile_entities,
        te_idx: 0,
        record_undo,
        old_snapshots: if record_undo {
            Vec::with_capacity(total_blocks)
        } else {
            Vec::new()
        },
        new_snapshots: if record_undo {
            Vec::with_capacity(total_blocks)
        } else {
            Vec::new()
        },
    }));
    let state_clone = state.clone();
    let task_id = std::sync::Arc::new(Mutex::new(0u32));
    let task_id_clone = task_id.clone();
    let mut description_owned = Some(description.clone());
    let mut dimension_owned = Some(dimension.clone());
    let player_name_owned = Some(player_name.clone());

    let id = pumpkin_plugin_api::scheduler::schedule_repeating_task(0, 1, move |server| {
        let world = match server.get_world_by_name(&dimension) {
            Some(w) => w,
            None => {
                ACTIVE_PASTES.fetch_sub(1, Ordering::Relaxed);
                let tid = *task_id_clone.lock().unwrap();
                pumpkin_plugin_api::scheduler::cancel_task(tid);
                return;
            }
        };

        let mut s = state_clone.lock().unwrap();

        let flags = world::BlockFlags::NOTIFY_LISTENERS
            | world::BlockFlags::FORCE_STATE
            | world::BlockFlags::SKIP_DROPS
            | world::BlockFlags::SKIP_REDSTONE_WIRE_STATE_REPLACEMENT
            | world::BlockFlags::SKIP_BLOCK_ADDED_CALLBACK;

        let first_tick = s.block_idx == 0;
        if first_tick {
            pumpkin_plugin_api::logging::log(
                pumpkin_plugin_api::logging::LogLevel::Info,
                &format!("[paste] task start: {} blocks queued", s.blocks.len()),
            );
        }

        let end = std::cmp::min(s.block_idx + blocks_per_tick, s.blocks.len());
        for i in s.block_idx..end {
            let (pos, state_id) = s.blocks[i];

            if s.record_undo {
                let old_id = world.get_block_state_id(pos);
                s.old_snapshots.push(BlockSnapshot {
                    pos,
                    state_id: old_id,
                });
                s.new_snapshots.push(BlockSnapshot { pos, state_id });
            }

            world.set_block_state(pos, state_id, flags);

            if first_tick && i == s.block_idx {
                let readback = world.get_block_state_id(pos);
                pumpkin_plugin_api::logging::log(
                    pumpkin_plugin_api::logging::LogLevel::Info,
                    &format!(
                        "[paste] first block ({},{},{}) set to {} readback={}",
                        pos.x, pos.y, pos.z, state_id, readback
                    ),
                );
            }
        }
        s.block_idx = end;

        if s.block_idx >= s.blocks.len() && s.te_idx < s.tile_entities.len() {
            let te_remaining = s.tile_entities.len() - s.te_idx;
            let te_batch = std::cmp::min(te_remaining, blocks_per_tick);
            for i in s.te_idx..s.te_idx + te_batch {
                let te = &s.tile_entities[i];
                let _ = world.set_block_entity_nbt(te.pos, &te.nbt);
            }
            s.te_idx += te_batch;
            return;
        }

        if s.block_idx >= s.blocks.len() && s.te_idx >= s.tile_entities.len() {
            s.blocks = Vec::new();
            s.tile_entities = Vec::new();

            if s.record_undo && !s.old_snapshots.is_empty() {
                let config = get_config();
                let entry = UndoEntry {
                    description: description_owned.take().unwrap_or_default(),
                    dimension: dimension_owned.take().unwrap_or_default(),
                    old_states: std::mem::take(&mut s.old_snapshots),
                    new_states: std::mem::take(&mut s.new_snapshots),
                };
                if let Some(ref mut histories) = *PLAYER_HISTORIES.lock().unwrap() {
                    let pname = player_name_owned
                        .clone()
                        .unwrap_or_default();
                    let history = histories
                        .entry(pname)
                        .or_insert_with(PlayerHistory::new);
                    history.push_undo(entry, config.max_undo_history);
                }
            }

            drop(s);

            ACTIVE_PASTES.fetch_sub(1, Ordering::Relaxed);

            if let Some(player) = server.get_player_by_name(&player_name) {
                player.send_system_message(
                    msg_success(&format!("Completed: {description}")),
                    false,
                );
            }

            let tid = *task_id_clone.lock().unwrap();
            pumpkin_plugin_api::scheduler::cancel_task(tid);
        }
    });

    *task_id.lock().unwrap() = id;
}
