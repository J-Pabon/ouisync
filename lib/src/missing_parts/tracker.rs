use super::MissingPartId;
use crate::{
    collections::{HashMap, HashSet},
    deadlock::BlockingMutex,
    protocol::BlockId,
};
use slab::Slab;
use std::{fmt, sync::Arc};
use tokio::sync::watch;

/// Helper for tracking required missing parts (blocks and index nodes).
#[derive(Clone)]
pub(crate) struct Tracker {
    shared: Arc<Shared>,
}

impl Tracker {
    pub fn new() -> Self {
        let (notify_tx, _) = watch::channel(());

        Self {
            shared: Arc::new(Shared {
                inner: BlockingMutex::new(Inner {
                    missing_parts: HashMap::default(),
                    offering_clients: Slab::new(),
                }),
                notify_tx,
            }),
        }
    }

    pub fn require_block(&self, block_id: BlockId) {
        self.require(MissingPartId::Block(block_id))
    }

    /// Mark the block with the given id as required.
    pub fn require(&self, part_id: MissingPartId) {
        let mut inner = self.shared.inner.lock().unwrap();

        let missing_part = inner
            .missing_parts
            .entry(part_id)
            .or_insert_with(|| MissingPartState {
                offering_clients: HashSet::default(),
                accepted_by: None,
                required: false,
                approved: false,
            });

        if missing_part.required {
            return;
        }

        tracing::trace!(?part_id, "require");

        missing_part.required = true;

        if !missing_part.offering_clients.is_empty() {
            self.shared.notify();
        }
    }

    pub fn approve_block(&self, block_id: BlockId) {
        self.approve(MissingPartId::Block(block_id))
    }

    /// Approve the part request if offered. This is called when `quota` is not `None`, otherwise
    /// blocks are pre-approved from `TrackerClient::offer(part_id, OfferState::Approved)`.
    pub fn approve(&self, part_id: MissingPartId) {
        let mut inner = self.shared.inner.lock().unwrap();

        let Some(missing_part) = inner.missing_parts.get_mut(&part_id) else {
            return;
        };

        if missing_part.approved {
            return;
        }

        tracing::trace!(?part_id, "approve");

        missing_part.approved = true;

        // If required and offered, notify the waiting acceptors.
        if missing_part.required && !missing_part.offering_clients.is_empty() {
            self.shared.notify();
        }
    }

    pub fn client(&self) -> TrackerClient {
        let client_id = self
            .shared
            .inner
            .lock()
            .unwrap()
            .offering_clients
            .insert(HashSet::default());

        let notify_rx = self.shared.notify_tx.subscribe();

        TrackerClient {
            shared: self.shared.clone(),
            client_id,
            notify_rx,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum OfferState {
    Pending,
    Approved,
}

pub(crate) struct TrackerClient {
    shared: Arc<Shared>,
    client_id: ClientId,
    notify_rx: watch::Receiver<()>,
}

impl TrackerClient {
    pub fn acceptor(&self) -> PartPromiseAcceptor {
        PartPromiseAcceptor {
            shared: self.shared.clone(),
            client_id: self.client_id,
            notify_rx: self.notify_rx.clone(),
        }
    }

    pub fn offer_block(&self, block_id: BlockId, state: OfferState) -> bool {
        self.offer(MissingPartId::Block(block_id), state)
    }

    /// Offer to request the given block by the client with `client_id` if it is, or will become,
    /// required and approved. Returns `true` if this block was offered for the first time (by any
    /// client), `false` if it was already offered before but not yet accepted or cancelled.
    pub fn offer(&self, part_id: MissingPartId, state: OfferState) -> bool {
        let mut inner = self.shared.inner.lock().unwrap();

        if !inner.offering_clients[self.client_id].insert(part_id) {
            // Already offered
            return false;
        }

        tracing::trace!(?part_id, ?state, "offer");

        let missing_part = inner
            .missing_parts
            .entry(part_id)
            .or_insert_with(|| MissingPartState {
                offering_clients: HashSet::default(),
                accepted_by: None,
                required: false,
                approved: false,
            });

        missing_part.offering_clients.insert(self.client_id);

        match state {
            OfferState::Approved => {
                missing_part.approved = true;
                self.shared.notify();
            }
            OfferState::Pending => (),
        }

        true
    }
}

impl Drop for TrackerClient {
    fn drop(&mut self) {
        let mut inner = self.shared.inner.lock().unwrap();
        let part_ids = inner.offering_clients.remove(self.client_id);
        let mut notify = false;

        for part_id in part_ids {
            // unwrap is ok because of the invariant in `Inner`
            let missing_part = inner.missing_parts.get_mut(&part_id).unwrap();

            missing_part.offering_clients.remove(&self.client_id);

            if missing_part.unaccept_by(self.client_id) {
                notify = true;
            }

            // TODO: if the block hasn't other offers and isn't required, remove it
        }

        if notify {
            self.shared.notify()
        }
    }
}

pub(crate) struct PartPromiseAcceptor {
    shared: Arc<Shared>,
    client_id: ClientId,
    notify_rx: watch::Receiver<()>,
}

impl PartPromiseAcceptor {
    /// Returns the next required, offered and approved block request. If there is no such request
    /// at the moment this function is called, waits until one appears.
    ///
    /// When the client receives this promise, it can request the block from the peer. The peer
    /// either responds and the client can fullfill the promise, or the promise can time out (or be
    /// dropped). If the latter, another will `accept` the promise.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe.
    pub async fn accept(&mut self) -> PartPromise {
        loop {
            if let Some(block_promise) = self.try_accept() {
                tracing::trace!(part_id = ?block_promise.part_id, "accept");
                return block_promise;
            }

            // unwrap is ok because the sender exists in self.shared.
            self.notify_rx.changed().await.unwrap();
        }
    }

    /// Returns the next required and offered block request or `None` if there is no such request
    /// currently.
    pub fn try_accept(&self) -> Option<PartPromise> {
        let mut inner = self.shared.inner.lock().unwrap();
        let inner = &mut *inner;

        // TODO: OPTIMIZE (but profile first) this linear lookup
        for part_id in &inner.offering_clients[self.client_id] {
            // unwrap is ok because of the invariant in `Inner`
            let missing_part = inner.missing_parts.get_mut(part_id).unwrap();

            if missing_part.required && missing_part.approved && missing_part.accepted_by.is_none()
            {
                missing_part.accepted_by = Some(self.client_id);

                return Some(PartPromise {
                    shared: self.shared.clone(),
                    client_id: self.client_id,
                    part_id: *part_id,
                    complete: false,
                });
            }
        }

        None
    }
}

/// Represents an accepted block request.
pub(crate) struct PartPromise {
    shared: Arc<Shared>,
    client_id: ClientId,
    part_id: MissingPartId,
    complete: bool,
}

impl PartPromise {
    #[cfg(test)]
    pub(crate) fn part_id(&self) -> &MissingPartId {
        &self.part_id
    }

    pub(crate) fn block_id(&self) -> &BlockId {
        match &self.part_id {
            MissingPartId::Block(block_id) => block_id,
        }
    }

    /// Mark the block request as successfully completed.
    pub fn complete(mut self) {
        let mut inner = self.shared.inner.lock().unwrap();

        let Some(missing_part) = inner.missing_parts.remove(&self.part_id) else {
            return;
        };

        for client_id in missing_part.offering_clients {
            if let Some(part_ids) = inner.offering_clients.get_mut(client_id) {
                part_ids.remove(&self.part_id);
            }
        }

        tracing::trace!(part_id = ?self.part_id, "complete");

        self.complete = true;
    }
}

impl fmt::Debug for PartPromise {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("PartPromise")
            .field("client_id", &self.client_id)
            .field("part_id", &self.part_id)
            .finish()
    }
}

impl Drop for PartPromise {
    fn drop(&mut self) {
        if self.complete {
            return;
        }

        let mut inner = self.shared.inner.lock().unwrap();

        let client = match inner.offering_clients.get_mut(self.client_id) {
            Some(client) => client,
            None => return,
        };

        if !client.remove(&self.part_id) {
            return;
        }

        // unwrap is ok because of the invariant in `Inner`
        let missing_part = inner.missing_parts.get_mut(&self.part_id).unwrap();
        missing_part.offering_clients.remove(&self.client_id);

        if missing_part.unaccept_by(self.client_id) {
            self.shared.notify();
        }
    }
}

struct Shared {
    inner: BlockingMutex<Inner>,
    notify_tx: watch::Sender<()>,
}

impl Shared {
    fn notify(&self) {
        self.notify_tx.send(()).unwrap_or(())
    }
}

// Invariant: for all `part_id` and `client_id` such that
//
//     missing_parts[part_id].offering_clients.contains(client_id)
//
// it must hold that
//
//     offering_clients[client_id].contains(part_id)
//
// and vice-versa.
struct Inner {
    missing_parts: HashMap<MissingPartId, MissingPartState>,
    offering_clients: Slab<HashSet<MissingPartId>>,
}

#[derive(Debug)]
struct MissingPartState {
    offering_clients: HashSet<ClientId>,
    accepted_by: Option<ClientId>,
    required: bool,
    approved: bool,
}

impl MissingPartState {
    fn unaccept_by(&mut self, client_id: ClientId) -> bool {
        if let Some(accepted_by) = &self.accepted_by {
            if accepted_by == &client_id {
                self.accepted_by = None;
                return true;
            }
        }

        false
    }
}

type ClientId = usize;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{collections::HashSet, protocol::Block, test_utils};
    use futures_util::future;
    use rand::{distributions::Standard, rngs::StdRng, seq::SliceRandom, Rng, SeedableRng};
    use std::{pin::pin, time::Duration};
    use test_strategy::proptest;
    use tokio::{select, sync::mpsc, sync::Barrier, task, time};

    #[test]
    fn simple() {
        let tracker = Tracker::new();

        let client = tracker.client();

        // Initially no blocks are returned
        assert!(client.acceptor().try_accept().is_none());

        // Offered but not required blocks are not returned
        let block0: Block = rand::random();
        client.offer(MissingPartId::Block(block0.id), OfferState::Approved);
        assert!(client.acceptor().try_accept().is_none());

        // Required but not offered blocks are not returned
        let block1: Block = rand::random();
        tracker.require_block(block1.id);
        assert!(client.acceptor().try_accept().is_none());

        // Required + offered blocks are returned...
        tracker.require_block(block0.id);
        assert_eq!(
            client
                .acceptor()
                .try_accept()
                .as_ref()
                .map(PartPromise::part_id),
            Some(&MissingPartId::Block(block0.id))
        );

        // ...but only once.
        assert!(client.acceptor().try_accept().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn simple_async() {
        let tracker = Tracker::new();

        let block: Block = rand::random();
        let client = tracker.client();
        let mut acceptor = client.acceptor();

        tracker.require_block(block.id);

        let (tx, mut rx) = mpsc::channel(1);

        let handle = tokio::task::spawn(async move {
            let mut accept_task = pin!(acceptor.accept());

            loop {
                select! {
                    block_promise = &mut accept_task => {
                        return *block_promise.part_id();
                    },
                    _ = tx.send(()) => {}
                }
            }
        });

        // Make sure acceptor started accepting.
        rx.recv().await.unwrap();

        client.offer(MissingPartId::Block(block.id), OfferState::Approved);

        let accepted_part_id = time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("timeout")
            .unwrap();

        assert_eq!(MissingPartId::Block(block.id), accepted_part_id);
    }

    #[test]
    fn fallback_on_cancel_after_accept() {
        let tracker = Tracker::new();

        let client0 = tracker.client();
        let client1 = tracker.client();

        let block: Block = rand::random();

        tracker.require_block(block.id);
        client0.offer(MissingPartId::Block(block.id), OfferState::Approved);
        client1.offer(MissingPartId::Block(block.id), OfferState::Approved);

        let block_promise = client0.acceptor().try_accept();
        assert_eq!(
            block_promise.as_ref().map(PartPromise::part_id),
            Some(&MissingPartId::Block(block.id))
        );
        assert!(client1.acceptor().try_accept().is_none());

        drop(block_promise);

        assert!(client0.acceptor().try_accept().is_none());
        assert_eq!(
            client1
                .acceptor()
                .try_accept()
                .as_ref()
                .map(PartPromise::part_id),
            Some(&MissingPartId::Block(block.id))
        );
    }

    #[test]
    fn fallback_on_client_drop_after_require_before_accept() {
        let tracker = Tracker::new();

        let client0 = tracker.client();
        let client1 = tracker.client();

        let block: Block = rand::random();

        client0.offer(MissingPartId::Block(block.id), OfferState::Approved);
        client1.offer(MissingPartId::Block(block.id), OfferState::Approved);

        tracker.require_block(block.id);

        drop(client0);

        assert_eq!(
            client1
                .acceptor()
                .try_accept()
                .as_ref()
                .map(PartPromise::part_id),
            Some(&MissingPartId::Block(block.id))
        );
    }

    #[test]
    fn fallback_on_client_drop_after_require_after_accept() {
        let tracker = Tracker::new();

        let client0 = tracker.client();
        let client1 = tracker.client();

        let block: Block = rand::random();

        client0.offer(MissingPartId::Block(block.id), OfferState::Approved);
        client1.offer(MissingPartId::Block(block.id), OfferState::Approved);

        tracker.require_block(block.id);

        let block_promise = client0.acceptor().try_accept();

        assert_eq!(
            block_promise.as_ref().map(PartPromise::part_id),
            Some(&MissingPartId::Block(block.id))
        );
        assert!(client1.acceptor().try_accept().is_none());

        drop(client0);

        assert_eq!(
            client1
                .acceptor()
                .try_accept()
                .as_ref()
                .map(PartPromise::part_id),
            Some(&MissingPartId::Block(block.id))
        );
    }

    #[test]
    fn fallback_on_client_drop_before_request() {
        let tracker = Tracker::new();

        let client0 = tracker.client();
        let client1 = tracker.client();

        let block: Block = rand::random();

        client0.offer(MissingPartId::Block(block.id), OfferState::Approved);
        client1.offer(MissingPartId::Block(block.id), OfferState::Approved);

        drop(client0);

        tracker.require_block(block.id);

        assert_eq!(
            client1
                .acceptor()
                .try_accept()
                .as_ref()
                .map(PartPromise::part_id),
            Some(&MissingPartId::Block(block.id))
        );
    }

    #[test]
    fn approve() {
        let tracker = Tracker::new();
        let client = tracker.client();

        let block: Block = rand::random();
        tracker.require_block(block.id);

        client.offer(MissingPartId::Block(block.id), OfferState::Pending);
        assert!(client.acceptor().try_accept().is_none());

        tracker.approve_block(block.id);
        assert_eq!(
            client
                .acceptor()
                .try_accept()
                .as_ref()
                .map(PartPromise::part_id),
            Some(&MissingPartId::Block(block.id))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn race() {
        let num_clients = 10;

        let tracker = Tracker::new();
        let clients: Vec<_> = (0..num_clients).map(|_| tracker.client()).collect();

        let block: Block = rand::random();

        tracker.require_block(block.id);

        for client in &clients {
            client.offer(MissingPartId::Block(block.id), OfferState::Approved);
        }

        // Make sure all clients stay alive until we are done so that any accepted requests are not
        // released prematurely.
        let barrier = Arc::new(Barrier::new(clients.len()));

        // Run the clients in parallel
        let handles = clients.into_iter().map(|client| {
            task::spawn({
                let barrier = barrier.clone();
                async move {
                    let block_promise = client.acceptor().try_accept();
                    let result = block_promise.as_ref().map(PartPromise::part_id).cloned();
                    barrier.wait().await;
                    result
                }
            })
        });

        let part_ids = future::try_join_all(handles).await.unwrap();

        // Exactly one client gets the block id
        let mut part_ids = part_ids.into_iter().flatten();
        assert_eq!(part_ids.next(), Some(MissingPartId::Block(block.id)));
        assert_eq!(part_ids.next(), None);
    }

    #[proptest]
    fn stress(
        #[strategy(1usize..100)] num_blocks: usize,
        #[strategy(test_utils::rng_seed_strategy())] rng_seed: u64,
    ) {
        stress_case(num_blocks, rng_seed)
    }

    fn stress_case(num_blocks: usize, rng_seed: u64) {
        let mut rng = StdRng::seed_from_u64(rng_seed);

        let tracker = Tracker::new();
        let client = tracker.client();

        let block_ids: Vec<BlockId> = (&mut rng).sample_iter(Standard).take(num_blocks).collect();

        enum Op {
            Require,
            Offer,
        }

        let mut ops: Vec<_> = block_ids
            .iter()
            .map(|block_id| (Op::Require, *block_id))
            .chain(block_ids.iter().map(|block_id| (Op::Offer, *block_id)))
            .collect();
        ops.shuffle(&mut rng);

        for (op, block_id) in ops {
            match op {
                Op::Require => {
                    tracker.require_block(block_id);
                }
                Op::Offer => {
                    client.offer_block(block_id, OfferState::Approved);
                }
            }
        }

        let mut block_promise = HashSet::with_capacity(block_ids.len());

        while let Some(block_id) = client
            .acceptor()
            .try_accept()
            .as_ref()
            .map(PartPromise::block_id)
        {
            block_promise.insert(*block_id);
        }

        assert_eq!(block_promise.len(), block_ids.len());

        for block_id in &block_ids {
            assert!(block_promise.contains(block_id));
        }
    }
}
