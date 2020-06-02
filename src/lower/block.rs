use super::instr::InstrIdx;

use cranelift_entity::entity_impl;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockIdx(u32);
entity_impl!(BlockIdx, "b");

#[derive(Debug)]
pub struct Block {
    pub idx: BlockIdx,
    /// First instruction of the block
    pub first_instr: InstrIdx,
    /// Last instruction of the block
    pub last_instr: InstrIdx,
    /// A block is filled is the last instruction (which is a control instruction, like 'ret' or
    /// 'jmp) is added.
    pub filled: bool,
    /// A block is selaed after adding all predecessors to it.
    pub sealed: bool,
}

pub const PLACEHOLDER_INSTR_IDX: u32 = u32::MAX - 1;

impl Block {
    pub fn new(idx: BlockIdx) -> Self {
        Block {
            idx,
            first_instr: InstrIdx::from_u32(PLACEHOLDER_INSTR_IDX),
            last_instr: InstrIdx::from_u32(PLACEHOLDER_INSTR_IDX),
            filled: false,
            sealed: false,
        }
    }
}

pub fn is_placeholder_instr(instr_idx: InstrIdx) -> bool {
    instr_idx.as_u32() == PLACEHOLDER_INSTR_IDX
}