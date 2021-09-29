mod client;
mod message;
mod message_broker;
mod object_stream;
mod replica_discovery;
mod server;
#[cfg(test)]
mod tests;

use self::{
    message_broker::MessageBroker,
    object_stream::TcpObjectStream,
    replica_discovery::{ReplicaDiscovery, RuntimeId},
};
use crate::{
    error::{Error, Result},
    index::Index,
    replica_id::ReplicaId,
    repository::Repository,
    scoped_task::{ScopedTaskHandle, ScopedTaskSet},
    tagged::{Local, Remote},
};
use futures_util::future;
use std::{
    collections::{hash_map::Entry, HashMap},
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};
use structopt::StructOpt;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{
        mpsc::{self, Receiver, Sender},
        Mutex, RwLock,
    },
};

#[derive(StructOpt, Debug)]
pub struct NetworkOptions {
    /// Port to listen on (0 for random)
    #[structopt(short, long, default_value = "0")]
    pub port: u16,

    /// IP address to bind to
    #[structopt(short, long, default_value = "0.0.0.0", value_name = "ip")]
    pub bind: IpAddr,

    /// Enable local discovery
    #[structopt(short, long)]
    pub enable_local_discovery: bool,
}

impl NetworkOptions {
    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(self.bind, self.port)
    }
}

impl Default for NetworkOptions {
    fn default() -> Self {
        Self {
            port: 0,
            bind: Ipv4Addr::UNSPECIFIED.into(),
            enable_local_discovery: true,
        }
    }
}

pub struct Network {
    inner: Arc<Inner>,
    local_addr: SocketAddr,
    tasks: ScopedTaskSet,
}

impl Network {
    pub async fn new(this_replica_id: ReplicaId, options: &NetworkOptions) -> Result<Self> {
        let tasks = ScopedTaskSet::default();

        let listener = TcpListener::bind(options.listen_addr())
            .await
            .map_err(Error::Network)?;
        let local_addr = listener.local_addr().map_err(Error::Network)?;
        let (forget_tx, forget_rx) = mpsc::channel(1);

        let inner = Inner {
            this_replica_id,
            message_brokers: Mutex::new(HashMap::new()),
            indices: RwLock::new(HashMap::new()),
            forget_tx,
            task_handle: tasks.handle().clone(),
        };

        let inner = Arc::new(inner);
        tasks.spawn(inner.clone().run_discovery(local_addr.port(), forget_rx));
        tasks.spawn(inner.clone().run_listener(listener));

        Ok(Self {
            inner,
            local_addr,
            tasks,
        })
    }

    pub fn local_addr(&self) -> &SocketAddr {
        &self.local_addr
    }

    /// Register a local repository to the network. This links the repository with all matching
    /// repositories of currently connected remote replicas as well as any replicas connected in
    /// the future. The repository is automatically deregistered when dropped.
    pub async fn register(&self, name: String, repository: &Repository) -> bool {
        let index = repository.index();

        match self.inner.indices.write().await.entry(name.to_owned()) {
            Entry::Vacant(entry) => {
                entry.insert(index.clone());
            }
            Entry::Occupied(_) => return false,
        }

        for broker in self.inner.message_brokers.lock().await.values() {
            create_link(broker, name.to_owned(), index.clone()).await;
        }

        // Remove the index from the `indices` map when it gets closed.
        self.tasks.spawn({
            let closed = index.subscribe().closed();
            let inner = self.inner.clone();

            async move {
                closed.await;
                inner.indices.write().await.remove(&name);
            }
        });

        true
    }
}

struct Inner {
    this_replica_id: ReplicaId,
    message_brokers: Mutex<HashMap<ReplicaId, MessageBroker>>,
    indices: RwLock<HashMap<String, Index>>,
    forget_tx: Sender<RuntimeId>,
    task_handle: ScopedTaskHandle,
}

impl Inner {
    async fn run_discovery(
        self: Arc<Self>,
        listener_port: u16,
        mut forget_rx: Receiver<RuntimeId>,
    ) {
        let (tx, mut rx) = mpsc::channel(1);
        let discovery = match ReplicaDiscovery::new(listener_port, tx) {
            Ok(discovery) => discovery,
            Err(error) => {
                log::error!("Failed to create ReplicaDiscovery: {}", error);
                return;
            }
        };

        let discover_task = async {
            while let Some((id, addr)) = rx.recv().await {
                self.task_handle
                    .spawn(self.clone().establish_outgoing_connection(addr, Some(id)))
            }
        };

        let forget_task = async {
            while let Some(id) = forget_rx.recv().await {
                discovery.forget(&id).await;
            }
        };

        future::join(discover_task, forget_task).await;
    }

    async fn run_listener(self: Arc<Self>, listener: TcpListener) {
        loop {
            let (socket, addr) = match listener.accept().await {
                Ok(pair) => pair,
                Err(error) => {
                    log::error!("Failed to accept incoming TCP connection: {}", error);
                    break;
                }
            };

            log::debug!("New incoming TCP connection: {}", addr);

            self.task_handle
                .spawn(self.clone().handle_new_connection(socket, None));
        }
    }

    async fn establish_outgoing_connection(
        self: Arc<Self>,
        addr: SocketAddr,
        discovery_id: Option<RuntimeId>,
    ) {
        let socket = match TcpStream::connect(addr).await {
            Ok(socket) => socket,
            Err(error) => {
                log::error!("Failed to create outgoing TCP connection: {}", error);
                return;
            }
        };

        log::debug!("New outgoing TCP connection: {}", addr);

        self.handle_new_connection(socket, discovery_id).await
    }

    async fn handle_new_connection(
        self: Arc<Self>,
        socket: TcpStream,
        discovery_id: Option<RuntimeId>,
    ) {
        let mut stream = TcpObjectStream::new(socket);
        let their_replica_id = match perform_handshake(&mut stream, &self.this_replica_id).await {
            Ok(replica_id) => replica_id,
            Err(error) => {
                log::error!("Failed to perform handshake: {}", error);
                return;
            }
        };

        let mut brokers = self.message_brokers.lock().await;

        match brokers.entry(their_replica_id) {
            Entry::Occupied(entry) => entry.get().add_connection(stream).await,
            Entry::Vacant(entry) => {
                log::info!("Connected to replica {:?}", their_replica_id);

                let broker = MessageBroker::new(
                    their_replica_id,
                    stream,
                    Box::pin(self.clone().on_finish(their_replica_id, discovery_id)),
                )
                .await;

                for (name, index) in self.indices.read().await.iter() {
                    create_link(&broker, name.to_owned(), index.clone()).await;
                }

                entry.insert(broker);
            }
        }
    }

    async fn on_finish(self: Arc<Self>, replica_id: ReplicaId, discovery_id: Option<RuntimeId>) {
        log::info!("Disconnected from replica {:?}", replica_id);

        self.message_brokers.lock().await.remove(&replica_id);

        if let Some(discovery_id) = discovery_id {
            self.forget_tx.send(discovery_id).await.unwrap_or(())
        }
    }
}

async fn perform_handshake(
    stream: &mut TcpObjectStream,
    this_replica_id: &ReplicaId,
) -> io::Result<ReplicaId> {
    stream.write(this_replica_id).await?;
    stream.read().await
}

async fn create_link(broker: &MessageBroker, name: String, index: Index) {
    // TODO: creating implicit link if the local and remote repository names are the same.
    // Eventually the links will be explicit.

    let local_name = Local::new(name.clone());
    let remote_name = Remote::new(name);

    broker.create_link(index, local_name, remote_name).await
}
