use super::block::{Block, BlockIdx};
use super::instr::{Instr, InstrIdx, Phi, PhiIdx, Value, ValueIdx};

use crate::cg_types::RepType;
use crate::ctx::VarId;

use cranelift_entity::{PrimaryMap, SecondaryMap};

#[derive(Debug)]
pub struct Fun {
    pub name: VarId,
    pub args: Vec<VarId>,
    pub blocks: PrimaryMap<BlockIdx, Block>,
    pub exit_blocks: Vec<BlockIdx>,
    pub values: PrimaryMap<ValueIdx, Value>,
    pub phis: PrimaryMap<PhiIdx, Phi>,
    pub instrs: PrimaryMap<InstrIdx, Instr>,
    pub succs: SecondaryMap<BlockIdx, Vec<BlockIdx>>,
    pub preds: SecondaryMap<BlockIdx, Vec<BlockIdx>>,
    pub value_use_sites: SecondaryMap<ValueIdx, Vec<ValueIdx>>, // use sites sorted
    pub block_phis: SecondaryMap<BlockIdx, Vec<PhiIdx>>,
    pub return_type: RepType,
}

#[derive(Debug)]
pub struct FunSig {
    pub name: VarId,
    pub args: Vec<VarId>,
    pub return_type: RepType,
}