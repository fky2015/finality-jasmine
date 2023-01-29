use crate::std::collections::BTreeMap;

use tracing::{info, trace, warn};
use tracing_attributes::instrument;

use super::*;

#[derive(Debug, Default, Clone)]
pub struct QC {
    height: u64,
    hash: Hash,
    signatures: BTreeMap<Id, Signature>,
}

impl From<crate::messages::QC<u64, String, u64, u64>> for QC {
    fn from(value: crate::messages::QC<u64, String, u64, u64>) -> Self {
        let mut signatures = BTreeMap::new();

        value.signatures.into_iter().for_each(|(id, signature)| {
            signatures.insert(id, signature);
        });

        Self {
            height: value.height,
            hash: value.hash,
            signatures,
        }
    }
}

impl QC {
    pub fn is_empty(&self) -> bool {
        self.signatures.is_empty()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Block {
    qc: QC,
    /// Hash
    hash: Hash,
    /// Block height
    number: BlockNumber,
    /// Parent hash
    parent: Hash,
    /// Key block or not
    key_block: bool,
}

impl Block {
    fn new_key_block(parent_block: &Block, hash: Hash, number: BlockNumber, qc: QC) -> Self {
        Self {
            qc,
            hash,
            number,
            parent: parent_block.hash.to_owned(),
            key_block: true,
        }
    }

    fn new_in_between_block(parent_block: &Block, hash: Hash, number: BlockNumber) -> Self {
        Self {
            qc: parent_block.qc.clone(),
            hash,
            number,
            parent: parent_block.hash.to_owned(),
            key_block: false,
        }
    }
}

/// A blockchain structure.
#[derive(Debug)]
pub struct DummyChain {
    inner: BTreeMap<Hash, Block>,
    finalized: (BlockNumber, Hash),
    children: BTreeMap<Hash, Vec<Hash>>,
    /// Will always be the key block.
    latest_voted: (BlockNumber, Hash),
}

impl DummyChain {
    pub fn new() -> Self {
        let mut inner = BTreeMap::new();

        let qc = QC {
            hash: GENESIS_HASH.to_string(),
            ..Default::default()
        };

        inner.insert(
            GENESIS_HASH.to_string(),
            Block {
                qc,
                number: 0,
                parent: GENESIS_HASH.to_string(),
                key_block: true,
                hash: GENESIS_HASH.to_string(),
            },
        );
        DummyChain {
            inner,
            finalized: (0, GENESIS_HASH.to_string()),
            children: BTreeMap::new(),
            latest_voted: (0, GENESIS_HASH.to_string()),
        }
    }

    /// Generate dummy blocks.
    ///
    /// WARNING: Only call this function when the chain is empty.
    pub(crate) fn generate_init_blocks(&mut self, n: usize) {
        let mut parent_block = self.get_genesis().clone();

        let key_block_interval = 3;
        for i in 0..n {
            let hash = format!("block-{i}");
            let block = if i % key_block_interval == 0 {
                let qc = QC::default();
                Block::new_key_block(&parent_block, hash, i as u64 + 1, qc)
            } else {
                Block::new_in_between_block(&parent_block, hash, i as u64 + 1)
            };

            self.push_block(block.clone());
            parent_block = block;
        }

        // First block.
        self.save_qc(self.get_genesis().qc.clone()).unwrap();
    }

    /// Get the genesis block.
    pub(crate) fn get_genesis(&self) -> &Block {
        self.inner.get(GENESIS_HASH).unwrap()
    }

    /// Add a chain to current chain.
    ///
    /// Used for test only.
    fn push_blocks(&mut self, blocks: &[Block]) {
        if blocks.is_empty() {
            return;
        }

        for i in blocks {
            self.push_block(i.clone());
        }
    }

    /// Add a block to current chain.
    fn push_block(&mut self, block: Block) {
        self.inner.insert(block.hash.to_owned(), block.clone());
        self.children
            .entry(block.parent)
            .or_default()
            .push(block.hash);
    }

    pub fn last_finalized(&self) -> (BlockNumber, Hash) {
        self.finalized.to_owned()
    }

    /// Get block after the last finalized block
    ///
    /// Always returns the key block.
    pub fn next_to_be_finalized(&self) -> Result<(BlockNumber, Hash), ()> {
        let (_, mut hash) = self.last_finalized();

        loop {
            hash = self.children.get(&hash).unwrap().get(0).unwrap().to_owned();
            let block = self.inner.get(&hash).ok_or(())?;
            if block.key_block {
                return Ok((block.number, hash));
            }
        }
    }

    /// Get the block to be voted on.
    ///
    /// Will return the last key block that have a QC.
    pub fn next_to_be_voted(&self) -> Result<(BlockNumber, Hash, (BlockNumber, Hash)), ()> {
        let (_, hash) = self.latest_voted.clone();

        let block = self.inner.get(&hash).ok_or(())?;
        Ok((block.number, hash, (block.qc.height, block.qc.hash.clone())))
    }

    #[instrument(skip(self), level = "trace")]
    pub(crate) fn save_qc(&mut self, qc: QC) -> Result<(), ()> {
        // find the next key block
        let (_, mut hash) = self.latest_voted.clone();

        loop {
            hash = self.children.get(&hash).unwrap().get(0).unwrap().to_owned();
            let block = self.inner.get(&hash).ok_or(())?;
            if block.key_block {
                break;
            }
        }

        // update the block
        let mut block = self.inner.get_mut(&hash).unwrap();
        block.qc = qc;
        self.latest_voted = (block.number, hash);

        Ok(())
    }

    /// Finalized a block.
    pub fn finalize_block(&mut self, block: Hash) -> bool {
        trace!("finalize_block: {:?}", block);
        if let Some(b) = self.inner.get(&block) {
            trace!("finalize block: {:?}", b);
            if self
                .inner
                .get(&b.parent)
                .map(|p| p.number < b.number)
                .unwrap_or(false)
            {
                self.finalized = (b.number, block);
                info!("new finalized = {:?}", self.finalized);
                return true;
            } else {
                warn!("block {} is not a descendent of {}", b.number, b.parent);
                panic!("block {} is not a descendent of {}", b.number, b.parent);
            }
        }

        false
    }

    pub fn parent_key_block(&self, hash: Hash) -> Option<(BlockNumber, Hash, (BlockNumber, Hash))> {
        let mut hash = self.inner.get(&hash).unwrap().parent.clone();
        loop {
            let block = self.inner.get(&hash).unwrap();
            if block.key_block {
                return Some((block.number, hash, (block.qc.height, block.qc.hash.clone())));
            }
            let new_hash = block.parent.clone();
            if new_hash == hash {
                return None;
            }
            hash = new_hash
        }
    }

    pub fn get_block(&self, hash: &Hash) -> Option<(BlockNumber, Hash, (BlockNumber, Hash))> {
        let block = self.inner.get(hash).unwrap();
        Some((
            block.number,
            hash.clone(),
            (block.qc.height, block.qc.hash.clone()),
        ))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn next_to_be_voted_will_paces() {
        let mut chain = DummyChain::new();
        chain.generate_init_blocks(10);

        let (number, hash, gen) = chain.next_to_be_voted().unwrap();
        assert_eq!(hash, "block-0");
        assert_eq!(number, 1);
        assert_eq!(gen, (0, GENESIS_HASH.to_owned()));

        chain
            .save_qc(QC {
                height: 1,
                hash: "block-0".to_owned(),
                signatures: BTreeMap::new(),
            })
            .unwrap();

        let (number, hash, qc) = chain.next_to_be_voted().unwrap();
        assert_eq!(hash, "block-3");
        assert_eq!(number, 4);
        assert_eq!(qc, (1, "block-0".to_owned()));
    }

    #[test]
    fn next_to_be_finalized_must_be_a_key_block() {
        let mut chain = DummyChain::new();
        chain.generate_init_blocks(10);

        let (number, hash) = chain.next_to_be_finalized().unwrap();
        assert_eq!(hash, "block-0");
        assert_eq!(number, 1);

        chain.finalize_block(hash);

        let (number, hash) = chain.next_to_be_finalized().unwrap();
        assert_eq!(hash, "block-3");
        assert_eq!(number, 4);
    }

    #[test]
    fn parent_key_block() {
        let mut chain = DummyChain::new();
        chain.generate_init_blocks(10);

        assert_eq!(chain.parent_key_block("block-0".to_owned()).unwrap().0, 0);
        assert_eq!(chain.parent_key_block("block-3".to_owned()).unwrap().0, 1);
        assert_eq!(chain.parent_key_block("block-6".to_owned()).unwrap().0, 4);
        assert_eq!(chain.parent_key_block("block-5".to_owned()).unwrap().0, 4);
        assert_eq!(chain.parent_key_block("block-9".to_owned()).unwrap().0, 7);
    }
}
