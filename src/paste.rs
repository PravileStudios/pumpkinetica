use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use pumpkin_plugin_api::common::BlockPos;
use pumpkin_plugin_api::logging::{self, LogLevel};
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

struct ChunkBatch {
    chunk_x: i32,
    chunk_z: i32,
    blocks: Vec<(BlockPos, u16)>,
}

struct PasteState {
    batches: Vec<ChunkBatch>,
    batch_idx: usize,
    block_offset: usize,
    tile_entities: Vec<TileEntityPlacement>,
    record_undo: bool,
    old_snapshots: Vec<BlockSnapshot>,
    new_snapshots: Vec<BlockSnapshot>,
}

fn build_chunk_batches(queue: Vec<BlockPlacement>) -> Vec<ChunkBatch> {
    let mut chunk_map: HashMap<(i32, i32), Vec<(BlockPos, u16)>> = HashMap::new();
    for p in queue {
        let cx = p.pos.x.div_euclid(16);
        let cz = p.pos.z.div_euclid(16);
        chunk_map
            .entry((cx, cz))
            .or_default()
            .push((p.pos, p.state_id));
    }
    chunk_map
        .into_iter()
        .map(|((cx, cz), blocks)| ChunkBatch {
            chunk_x: cx,
            chunk_z: cz,
            blocks,
        })
        .collect()
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
        format!("Paste '{schematic_name}' at ({}, {}, {})", origin.x, origin.y, origin.z),
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
    let batches = build_chunk_batches(queue);
    logging::log(LogLevel::Info, &format!(
        "[PSchematics] schedule_block_op: {} blocks, {} chunk batches, dim={}, desc={}",
        total_blocks, batches.len(), dimension, description
    ));

    let state = std::sync::Arc::new(Mutex::new(PasteState {
        batches,
        batch_idx: 0,
        block_offset: 0,
        tile_entities,
        record_undo,
        old_snapshots: Vec::new(),
        new_snapshots: Vec::new(),
    }));
    let state_clone = state.clone();
    let task_id = std::sync::Arc::new(Mutex::new(0u32));
    let task_id_clone = task_id.clone();
    let description_clone = description.clone();
    let dimension_clone = dimension.clone();
    let player_name_clone = player_name.clone();

    let id = pumpkin_plugin_api::scheduler::schedule_repeating_task(0, 1, move |server| {
        let world = match server.get_world_by_name(&dimension) {
            Some(w) => w,
            None => {
                logging::log(LogLevel::Error, &format!(
                    "[PSchematics] World '{}' not found, aborting paste", dimension
                ));
                ACTIVE_PASTES.fetch_sub(1, Ordering::Relaxed);
                let tid = *task_id_clone.lock().unwrap();
                pumpkin_plugin_api::scheduler::cancel_task(tid);
                return;
            }
        };

        let mut s = state_clone.lock().unwrap();
        let mut remaining = blocks_per_tick;

        while remaining > 0 && s.batch_idx < s.batches.len() {
            let chunk_x = s.batches[s.batch_idx].chunk_x;
            let chunk_z = s.batches[s.batch_idx].chunk_z;
            let batch_len = s.batches[s.batch_idx].blocks.len();
            let blocks_left = batch_len - s.block_offset;
            let to_place = std::cmp::min(remaining, blocks_left);
            let offset = s.block_offset;
            let record_undo = s.record_undo;

            let block_slice: Vec<(BlockPos, u16)> =
                s.batches[s.batch_idx].blocks[offset..offset + to_place].to_vec();

            match world.get_chunk(chunk_x, chunk_z) {
                Some(chunk) => {
                    for &(pos, state_id) in &block_slice {
                        let local = BlockPos {
                            x: pos.x.rem_euclid(16),
                            y: pos.y,
                            z: pos.z.rem_euclid(16),
                        };

                        if record_undo {
                            let old_id = chunk.get_block_state_id(local);
                            s.old_snapshots.push(BlockSnapshot {
                                pos,
                                state_id: old_id,
                            });
                            s.new_snapshots.push(BlockSnapshot { pos, state_id });
                        }

                        chunk.set_block_state(local, state_id);
                    }
                }
                None => {
                    let flags = world::BlockFlags::FORCE_STATE
                        | world::BlockFlags::SKIP_DROPS
                        | world::BlockFlags::SKIP_REDSTONE_WIRE_STATE_REPLACEMENT
                        | world::BlockFlags::SKIP_BLOCK_ADDED_CALLBACK;
                    for &(pos, state_id) in &block_slice {
                        if record_undo {
                            let old_id = world.get_block_state_id(pos);
                            s.old_snapshots.push(BlockSnapshot {
                                pos,
                                state_id: old_id,
                            });
                            s.new_snapshots.push(BlockSnapshot { pos, state_id });
                        }

                        world.set_block_state(pos, state_id, flags);
                    }
                }
            }

            remaining -= to_place;

            if offset + to_place >= batch_len {
                s.batch_idx += 1;
                s.block_offset = 0;
            } else {
                s.block_offset += to_place;
            }
        }

        if s.batch_idx >= s.batches.len() {
            for te in s.tile_entities.drain(..) {
                let _ = world.set_block_entity_nbt(te.pos, &te.nbt);
            }

            if s.record_undo && !s.old_snapshots.is_empty() {
                let config = get_config();
                let entry = UndoEntry {
                    description: description_clone.clone(),
                    dimension: dimension_clone.clone(),
                    old_states: std::mem::take(&mut s.old_snapshots),
                    new_states: std::mem::take(&mut s.new_snapshots),
                };
                if let Some(ref mut histories) = *PLAYER_HISTORIES.lock().unwrap() {
                    let history = histories
                        .entry(player_name_clone.clone())
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
