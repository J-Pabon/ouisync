use super::{
    message::Message,
    object_stream::{TcpObjectReader, TcpObjectWriter},
};
use std::{
    collections::{hash_map::Entry, HashMap},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex as SyncMutex,
    },
};
use tokio::{
    select,
    sync::{mpsc, Mutex},
    task,
};

/// Wrapper for arbitrary number of `TcpObjectReader`s which reads from all of them simultaneously.
pub(super) struct MultiReader {
    tx: mpsc::Sender<Option<Message>>,
    // Wrapping these in Mutex and RwLock to have the `add` and `read` methods non mutable.  That
    // in turn is desirable to be able to call the two functions from different coroutines. Note
    // that we don't want to wrap this whole struct in a Mutex/RwLock because we don't want the add
    // function to be blocking.
    rx: Mutex<mpsc::Receiver<Option<Message>>>,
    count: AtomicUsize,
}

impl MultiReader {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(1);
        Self {
            tx,
            rx: Mutex::new(rx),
            count: AtomicUsize::new(0),
        }
    }

    pub fn add(&self, mut reader: TcpObjectReader) {
        let tx = self.tx.clone();

        // Using `SeqCst` here to be on the safe side although a weaker ordering would probably
        // suffice here (also in the `read` method).
        self.count.fetch_add(1, Ordering::SeqCst);

        task::spawn(async move {
            loop {
                select! {
                    result = reader.read() => {
                        if let Ok(message) = result {
                            tx.send(Some(message)).await.unwrap_or(())
                        } else {
                            tx.send(None).await.unwrap_or(());
                            break;
                        }
                    },
                    _ = tx.closed() => break,
                }
            }
        });
    }

    pub async fn read(&self) -> Option<Message> {
        loop {
            if self.count.load(Ordering::SeqCst) == 0 {
                return None;
            }

            match self.rx.lock().await.recv().await {
                Some(Some(message)) => return Some(message),
                Some(None) => {
                    self.count.fetch_sub(1, Ordering::SeqCst);
                }
                None => {
                    // This would mean that all senders were closed, but that can't happen because
                    // `self.tx` still exists.
                    unreachable!()
                }
            }
        }
    }
}

/// Wrapper for arbitrary number of `TcpObjectWriter`s which writes to the first available one.
pub(super) struct MultiWriter {
    // Using Mutexes and RwLocks here because we want the `add` and `write` functions to be const.
    // That will allow us to call them from two different coroutines. Note that we don't want this
    // whole structure to wrap because we don't want the `add` function to be blocking.
    next_id: AtomicUsize,
    writers: std::sync::RwLock<HashMap<usize, Arc<Mutex<TcpObjectWriter>>>>,
}

impl MultiWriter {
    pub fn new() -> Self {
        Self {
            next_id: AtomicUsize::new(0),
            writers: std::sync::RwLock::new(HashMap::new()),
        }
    }

    pub fn add(&self, writer: TcpObjectWriter) {
        // `Relaxed` ordering should be sufficient here because this is just a simple counter.
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        self.writers
            .write()
            .unwrap()
            .insert(id, Arc::new(Mutex::new(writer)));
    }

    pub async fn write(&self, message: &Message) -> bool {
        while let Some((id, writer)) = self.pick_writer().await {
            if writer.lock().await.write(message).await.is_ok() {
                return true;
            }

            self.writers.write().unwrap().remove(&id);
        }

        false
    }

    async fn pick_writer(&self) -> Option<(usize, Arc<Mutex<TcpObjectWriter>>)> {
        self.writers
            .read()
            .unwrap()
            .iter()
            .next()
            .map(|(k, v)| (*k, v.clone()))
    }
}

/// Prevents establishing duplicate connections.
pub(super) struct ConnectionDeduplicator {
    next_id: AtomicU64,
    connections: Arc<SyncMutex<HashMap<SocketAddr, u64>>>,
}

impl ConnectionDeduplicator {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(0),
            connections: Arc::new(SyncMutex::new(HashMap::new())),
        }
    }

    /// Attempt to reserve a connection to the given peer. If the connection hasn't been reserved
    /// yet, it returns a `ConnectionPermit` which keeps the connection reserved as long as it
    /// lives. Otherwise it returns `None`. To release a connection the permit needs to be dropped.
    pub fn reserve(&self, addr: SocketAddr) -> Option<ConnectionPermit> {
        let id = if let Entry::Vacant(entry) = self.connections.lock().unwrap().entry(addr) {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            entry.insert(id);
            id
        } else {
            return None;
        };

        Some(ConnectionPermit {
            connections: self.connections.clone(),
            addr,
            id,
        })
    }
}

/// Connection permit that prevents another connection to the same peer (socket address) to be
/// established as long as it remains in scope.
pub(super) struct ConnectionPermit {
    connections: Arc<SyncMutex<HashMap<SocketAddr, u64>>>,
    addr: SocketAddr,
    id: u64,
}

impl ConnectionPermit {
    /// Split the permit into two halves where dropping any of them releases the whole permit.
    /// This is useful when the connection needs to be split into a reader and a writer Then if any
    /// of them closes, the whole connection closes. So both the reader and the writer should be
    /// associated with one half of the permit so that when any of them closes, the permit is
    /// released.
    pub fn split(self) -> (ConnectionPermitHalf, ConnectionPermitHalf) {
        (
            ConnectionPermitHalf(Self {
                connections: self.connections.clone(),
                addr: self.addr,
                id: self.id,
            }),
            ConnectionPermitHalf(self),
        )
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        if let Entry::Occupied(entry) = self.connections.lock().unwrap().entry(self.addr) {
            if *entry.get() == self.id {
                entry.remove();
            }
        }
    }
}

/// Half of a connection permit. Dropping it drops the whole permit.
/// See [`ConnectionPermit::split`] for more details.
pub(super) struct ConnectionPermitHalf(ConnectionPermit);
