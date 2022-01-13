use super::{
    message::{Request, Response},
    message_broker::ServerStream,
};
use crate::{
    block::{self, BlockId, BLOCK_SIZE},
    crypto::{sign::PublicKey, Hash},
    error::{Error, Result},
    index::{Index, InnerNode, LeafNode},
};
use tokio::select;

pub(crate) struct Server {
    index: Index,
    stream: ServerStream,
}

impl Server {
    pub fn new(index: Index, stream: ServerStream) -> Self {
        Self { index, stream }
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut subscription = self.index.subscribe();

        // send initial branches
        let branch_ids: Vec<_> = self.index.branches().await.keys().copied().collect();
        for branch_id in branch_ids {
            self.handle_branch_changed(branch_id).await?;
        }

        loop {
            select! {
                request = self.stream.recv() => {
                    let request = if let Some(request) = request {
                        request
                    } else {
                        break;
                    };

                    self.handle_request(request).await?
                }
                branch_id = subscription.recv() => {
                    let branch_id = if let Ok(branch_id) = branch_id {
                        branch_id
                    } else {
                        break;
                    };

                    self.handle_branch_changed(branch_id).await?
                }
            }
        }

        Ok(())
    }

    async fn handle_branch_changed(&mut self, branch_id: PublicKey) -> Result<()> {
        let branches = self.index.branches().await;
        let branch = if let Some(branch) = branches.get(&branch_id) {
            branch
        } else {
            // branch was removed after the notification was fired.
            return Ok(());
        };

        let root_node = branch.root().await;

        if !root_node.summary.is_complete() {
            // send only complete branches
            return Ok(());
        }

        let response = Response::RootNode {
            proof: root_node.proof.into(),
            version_vector: root_node.versions.clone(),
            summary: root_node.summary,
        };

        // Don't hold the locks while sending is in progress.
        drop(root_node);
        drop(branches);

        self.stream.send(response).await;

        Ok(())
    }

    async fn handle_request(&mut self, request: Request) -> Result<()> {
        match request {
            Request::ChildNodes(parent_hash) => self.handle_child_nodes(parent_hash).await,
            Request::Block(id) => self.handle_block(id).await,
        }
    }

    async fn handle_child_nodes(&self, parent_hash: Hash) -> Result<()> {
        let mut conn = self.index.pool.acquire().await?;

        // At most one of these will be non-empty.
        let inner_nodes = InnerNode::load_children(&mut conn, &parent_hash).await?;
        let leaf_nodes = LeafNode::load_children(&mut conn, &parent_hash).await?;

        if !inner_nodes.is_empty() {
            self.stream.send(Response::InnerNodes(inner_nodes)).await;
        }

        if !leaf_nodes.is_empty() {
            self.stream.send(Response::LeafNodes(leaf_nodes)).await;
        }

        Ok(())
    }

    async fn handle_block(&self, id: BlockId) -> Result<()> {
        let mut content = vec![0; BLOCK_SIZE].into_boxed_slice();

        let auth_tag =
            match block::read(&mut *self.index.pool.acquire().await?, &id, &mut content).await {
                Ok(auth_tag) => auth_tag,
                Err(Error::BlockNotFound(_)) => {
                    // This is probably a request to an already deleted orphaned block from an outdated
                    // branch. It should be safe to ingore this as the client will request the correct
                    // blocks when it becomes up to date to our latest branch.
                    log::warn!("requested block {:?} not found", id);
                    return Ok(());
                }
                Err(error) => return Err(error),
            };

        self.stream
            .send(Response::Block {
                id,
                content,
                auth_tag,
            })
            .await;

        Ok(())
    }
}
