use std::collections::HashMap;

use pumpkin_plugin_api::common::BlockPos;
use pumpkin_plugin_api::world::World;

use crate::paste::BlockPlacement;
use crate::{REVERSE_REGISTRY, resolve_and_register};

pub(crate) struct Clipboard {
    pub blocks: Vec<u16>,
    pub size_x: i32,
    pub size_y: i32,
    pub size_z: i32,
    pub offset: BlockPos,
}

impl Clipboard {
    fn idx(&self, x: i32, y: i32, z: i32) -> usize {
        (y * self.size_x * self.size_z + z * self.size_x + x) as usize
    }

    pub fn to_work_queue(&self, origin: BlockPos) -> Vec<BlockPlacement> {
        let mut queue = Vec::new();
        for y in 0..self.size_y {
            for z in 0..self.size_z {
                for x in 0..self.size_x {
                    let state_id = self.blocks[self.idx(x, y, z)];
                    if state_id == 0 {
                        continue;
                    }
                    queue.push(BlockPlacement {
                        pos: BlockPos {
                            x: origin.x - self.offset.x + x,
                            y: origin.y - self.offset.y + y,
                            z: origin.z - self.offset.z + z,
                        },
                        state_id,
                    });
                }
            }
        }
        queue
    }
}

pub(crate) fn read_selection_chunk_batched(
    world: &World,
    min: BlockPos,
    max: BlockPos,
    sx: i32,
    sy: i32,
    sz: i32,
) -> Vec<u16> {
    let total = (sx * sy * sz) as usize;
    let mut blocks = vec![0u16; total];

    let min_cx = min.x.div_euclid(16);
    let max_cx = max.x.div_euclid(16);
    let min_cz = min.z.div_euclid(16);
    let max_cz = max.z.div_euclid(16);

    for cx in min_cx..=max_cx {
        for cz in min_cz..=max_cz {
            let chunk = world.get_chunk(cx, cz);

            let chunk_min_x = cx * 16;
            let chunk_max_x = chunk_min_x + 15;
            let chunk_min_z = cz * 16;
            let chunk_max_z = chunk_min_z + 15;

            let overlap_min_x = min.x.max(chunk_min_x);
            let overlap_max_x = max.x.min(chunk_max_x);
            let overlap_min_z = min.z.max(chunk_min_z);
            let overlap_max_z = max.z.min(chunk_max_z);

            for y in min.y..=max.y {
                for z in overlap_min_z..=overlap_max_z {
                    for x in overlap_min_x..=overlap_max_x {
                        let state_id = match &chunk {
                            Some(c) => {
                                let local = BlockPos {
                                    x: x.rem_euclid(16),
                                    y,
                                    z: z.rem_euclid(16),
                                };
                                c.get_block_state_id(local)
                            }
                            None => world.get_block_state_id(BlockPos { x, y, z }),
                        };
                        let rel_x = x - min.x;
                        let rel_y = y - min.y;
                        let rel_z = z - min.z;
                        let idx = (rel_y * sx * sz + rel_z * sx + rel_x) as usize;
                        blocks[idx] = state_id;
                    }
                }
            }
        }
    }

    blocks
}

pub(crate) fn rotate_clipboard(clip: &mut Clipboard, degrees: i32) {
    let (sx, sy, sz) = (clip.size_x, clip.size_y, clip.size_z);
    let (new_sx, new_sz) = match degrees {
        90 | 270 => (sz, sx),
        180 => (sx, sz),
        _ => return,
    };

    let mut new_blocks = vec![0u16; clip.blocks.len()];
    for y in 0..sy {
        for z in 0..sz {
            for x in 0..sx {
                let old_idx = (y * sx * sz + z * sx + x) as usize;
                let (nx, nz) = match degrees {
                    90 => (sz - 1 - z, x),
                    180 => (sx - 1 - x, sz - 1 - z),
                    270 => (z, sx - 1 - x),
                    _ => unreachable!(),
                };
                let new_idx = (y * new_sx * new_sz + nz * new_sx + nx) as usize;
                new_blocks[new_idx] = rotate_block_state(clip.blocks[old_idx], degrees);
            }
        }
    }

    clip.blocks = new_blocks;
    clip.size_x = new_sx;
    clip.size_z = new_sz;

    let (ox, oz) = (clip.offset.x, clip.offset.z);
    match degrees {
        90 => {
            clip.offset.x = -oz;
            clip.offset.z = ox;
        }
        180 => {
            clip.offset.x = -ox;
            clip.offset.z = -oz;
        }
        270 => {
            clip.offset.x = oz;
            clip.offset.z = -ox;
        }
        _ => {}
    }
}

#[derive(Clone, Copy)]
pub(crate) enum FlipAxis {
    X,
    Z,
}

pub(crate) fn flip_clipboard(clip: &mut Clipboard, axis: FlipAxis) {
    let (sx, sy, sz) = (clip.size_x, clip.size_y, clip.size_z);
    let mut new_blocks = vec![0u16; clip.blocks.len()];

    for y in 0..sy {
        for z in 0..sz {
            for x in 0..sx {
                let old_idx = (y * sx * sz + z * sx + x) as usize;
                let (nx, nz) = match axis {
                    FlipAxis::X => (sx - 1 - x, z),
                    FlipAxis::Z => (x, sz - 1 - z),
                };
                let new_idx = (y * sx * sz + nz * sx + nx) as usize;
                new_blocks[new_idx] = flip_block_state(clip.blocks[old_idx], axis);
            }
        }
    }

    clip.blocks = new_blocks;

    match axis {
        FlipAxis::X => clip.offset.x = -(clip.offset.x),
        FlipAxis::Z => clip.offset.z = -(clip.offset.z),
    }
}

fn rotate_block_state(state_id: u16, degrees: i32) -> u16 {
    if state_id == 0 {
        return 0;
    }

    let reg = REVERSE_REGISTRY.lock().unwrap();
    let Some(ref map) = *reg else { return state_id };
    let Some(entry) = map.get(&state_id) else {
        return state_id;
    };

    let mut props = entry.properties.clone();
    let mut changed = false;

    if let Some(facing) = props.get("facing")
        && let Some(new_facing) = rotate_facing(facing, degrees)
    {
        props.insert("facing".into(), new_facing.into());
        changed = true;
    }

    if let Some(axis) = props.get("axis")
        && let Some(new_axis) = rotate_axis(axis, degrees)
    {
        props.insert("axis".into(), new_axis.into());
        changed = true;
    }

    if let Some(rotation) = props.get("rotation")
        && let Ok(r) = rotation.parse::<i32>()
    {
        let steps = degrees / 90;
        let new_r = ((r + steps * 4) % 16 + 16) % 16;
        props.insert("rotation".into(), new_r.to_string());
        changed = true;
    }

    if !changed {
        return state_id;
    }

    let name = entry.name.clone();
    drop(reg);

    let prop_vec: Vec<(String, String)> = props.into_iter().collect();
    resolve_and_register(&name, &prop_vec).unwrap_or(state_id)
}

fn flip_block_state(state_id: u16, axis: FlipAxis) -> u16 {
    if state_id == 0 {
        return 0;
    }

    let reg = REVERSE_REGISTRY.lock().unwrap();
    let Some(ref map) = *reg else { return state_id };
    let Some(entry) = map.get(&state_id) else {
        return state_id;
    };

    let mut props = entry.properties.clone();
    let mut changed = false;

    if let Some(facing) = props.get("facing") {
        let new_facing = match axis {
            FlipAxis::X => match facing.as_str() {
                "east" => Some("west"),
                "west" => Some("east"),
                _ => None,
            },
            FlipAxis::Z => match facing.as_str() {
                "north" => Some("south"),
                "south" => Some("north"),
                _ => None,
            },
        };
        if let Some(nf) = new_facing {
            props.insert("facing".into(), nf.into());
            changed = true;
        }
    }

    if !changed {
        return state_id;
    }

    let name = entry.name.clone();
    drop(reg);

    let prop_vec: Vec<(String, String)> = props.into_iter().collect();
    resolve_and_register(&name, &prop_vec).unwrap_or(state_id)
}

fn rotate_facing(facing: &str, degrees: i32) -> Option<&'static str> {
    let order = ["north", "east", "south", "west"];
    let idx = order.iter().position(|&f| f == facing)?;
    let steps = ((degrees / 90) % 4 + 4) % 4;
    Some(order[(idx + steps as usize) % 4])
}

fn rotate_axis(axis: &str, degrees: i32) -> Option<&'static str> {
    if degrees % 180 == 0 {
        return None;
    }
    match axis {
        "x" => Some("z"),
        "z" => Some("x"),
        _ => None,
    }
}

pub(crate) fn clipboard_to_schem_data(
    clip: &Clipboard,
    reverse_registry: &HashMap<u16, crate::parser::PaletteEntry>,
) -> (HashMap<String, u16>, Vec<u16>, usize) {
    let mut palette_map: HashMap<u16, u16> = HashMap::new();
    let mut palette_strings: HashMap<String, u16> = HashMap::new();
    let mut next_idx: u16 = 0;
    let mut unresolved = 0usize;

    let mut indices = Vec::with_capacity(clip.blocks.len());

    for &state_id in &clip.blocks {
        let palette_idx = if let Some(&existing) = palette_map.get(&state_id) {
            existing
        } else {
            let block_str = if state_id == 0 {
                "minecraft:air".to_string()
            } else if let Some(entry) = reverse_registry.get(&state_id) {
                if entry.properties.is_empty() {
                    entry.name.clone()
                } else {
                    let props: Vec<String> = entry
                        .properties
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    format!("{}[{}]", entry.name, props.join(","))
                }
            } else {
                unresolved += 1;
                "minecraft:stone".to_string()
            };

            let idx = if let Some(&existing_idx) = palette_strings.get(&block_str) {
                existing_idx
            } else {
                let idx = next_idx;
                palette_strings.insert(block_str, idx);
                next_idx += 1;
                idx
            };
            palette_map.insert(state_id, idx);
            idx
        };
        indices.push(palette_idx);
    }

    (palette_strings, indices, unresolved)
}
