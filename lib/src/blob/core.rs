use super::{
    operations::{load_block, Operations},
    {Cursor, OpenBlock},
};
use crate::{
    block::BlockId, branch::Branch, crypto::NonceSequence, error::Result, locator::Locator,
};
use std::{fmt, mem};

pub(crate) struct Core {
    pub branch: Branch,
    pub head_locator: Locator,
    pub nonce_sequence: NonceSequence,
    pub len: u64,
    pub len_dirty: bool,
}

impl Core {
    pub async fn open_first_block(&self) -> Result<OpenBlock> {
        if self.len == 0 {
            return Ok(OpenBlock::new_head(self.head_locator, &self.nonce_sequence));
        }

        // NOTE: no need to commit this transaction because we are only reading here.
        let mut tx = self.branch.db_pool().begin().await?;

        let (id, buffer, auth_tag) = load_block(
            &mut tx,
            self.branch.data(),
            self.branch.cryptor(),
            &self.head_locator,
        )
        .await?;

        let mut content = Cursor::new(buffer);
        content.pos = self.header_size();

        let nonce = self.nonce_sequence.get(0);

        self.branch
            .cryptor()
            .decrypt(&nonce, id.as_ref(), &mut content, &auth_tag)?;

        Ok(OpenBlock {
            locator: self.head_locator,
            id,
            content,
            dirty: false,
        })
    }

    pub async fn first_block_id(branch: &Branch, head_locator: Locator) -> Result<BlockId> {
        // NOTE: no need to commit this transaction because we are only reading here.
        let mut tx = branch.db_pool().begin().await?;
        branch
            .data()
            .get(&mut tx, &head_locator.encode(branch.cryptor()))
            .await
    }

    /// Length of this blob in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn operations<'a>(&'a mut self, current_block: &'a mut OpenBlock) -> Operations<'a> {
        Operations::new(self, current_block)
    }

    pub fn header_size(&self) -> usize {
        self.nonce_sequence.prefix().len() + mem::size_of_val(&self.len)
    }
}

impl fmt::Debug for Core {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("blob::Core")
            .field("head_locator", &self.head_locator)
            .finish()
    }
}
