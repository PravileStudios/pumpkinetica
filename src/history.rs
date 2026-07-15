use pumpkin_plugin_api::common::BlockPos;

pub(crate) struct BlockSnapshot {
    pub pos: BlockPos,
    pub state_id: u16,
}

pub(crate) struct UndoEntry {
    pub description: String,
    pub dimension: String,
    pub old_states: Vec<BlockSnapshot>,
    pub new_states: Vec<BlockSnapshot>,
}

pub(crate) struct PlayerHistory {
    pub undo_stack: Vec<UndoEntry>,
    pub redo_stack: Vec<UndoEntry>,
}

impl PlayerHistory {
    pub fn new() -> Self {
        Self {
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    pub fn push_undo(&mut self, entry: UndoEntry, max_depth: usize) {
        if self.undo_stack.len() >= max_depth {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(entry);
        self.redo_stack.clear();
    }
}
