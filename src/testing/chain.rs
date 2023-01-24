use crate::std::collections::BTreeMap;

use super::*;

#[derive(Debug, Default, Clone)]
pub struct QC {
    height: u64,
    hash: Hash,
    signatures: BTreeMap<Id, Signature>,
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
        inner.insert(
            GENESIS_HASH.to_string(),
            Block {
                qc: QC::default(),
                number: 0,
                parent: NULL_HASH.to_string(),
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
            let hash = format!("block-{}", i);
            let block = if i % key_block_interval == 0 {
                let qc = QC::default();
                Block::new_key_block(&parent_block, hash, i as u64 + 1, qc)
            } else {
                Block::new_in_between_block(&parent_block, hash, i as u64 + 1)
            };

            self.push_block(block.clone());
            parent_block = block;
        }
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

        Ok(())
    }

    /// Finalized a block.
    pub fn finalize_block(&mut self, block: Hash) -> bool {
        #[cfg(feature = "std")]
        log::trace!("finalize_block: {:?}", block);
        if let Some(b) = self.inner.get(&block) {
            #[cfg(feature = "std")]
            log::trace!("finalize block: {:?}", b);
            if self
                .inner
                .get(&b.parent)
                .map(|p| p.number < b.number)
                .unwrap_or(false)
            {
                self.finalized = (b.number, block);
                #[cfg(feature = "std")]
                log::info!("new finalized = {:?}", self.finalized);
                return true;
            } else {
                panic!("block {} is not a descendent of {}", b.number, b.parent);
            }
        }

        false
    }
}

#[cfg(test)]
mod test {
    use super::*;

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
}
