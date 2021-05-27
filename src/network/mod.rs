mod client;
mod message;
mod message_broker;
mod object_stream;
mod replica_discovery;
mod server;

use self::{
    message_broker::MessageBroker,
    object_stream::ObjectStream,
    replica_discovery::{ReplicaDiscovery, RuntimeId},
};
use crate::{
    replica_id::ReplicaId,
    scoped_task_set::{ScopedTaskHandle, ScopedTaskSet},
    Index,
};
use std::{
    collections::{HashMap, HashSet},
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};
use structopt::StructOpt;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Mutex,
};

pub const DEFAULT_PORT: u16 = 65535;

#[derive(StructOpt)]
pub struct NetworkOptions {
    /// Port to listen on
    #[structopt(short, long, default_value = "65535")]
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
            port: DEFAULT_PORT,
            bind: Ipv4Addr::UNSPECIFIED.into(),
            enable_local_discovery: true,
        }
    }
}

pub struct Network {
    _tasks: ScopedTaskSet,
}

impl Network {
    pub async fn new(index: Index, options: NetworkOptions) -> io::Result<Self> {
        let tasks = ScopedTaskSet::default();
        let task_handle = tasks.handle().clone();

        let inner = Arc::new(Inner {
            this_replica_id: ReplicaId::random(),
            message_brokers: Mutex::new(HashMap::new()),
            to_forget: Mutable::new(HashSet::new()),
            task_handle,
            index,
        });

        inner.start(options.listen_addr()).await?;

        Ok(Self { _tasks: tasks })
    }
}

struct Inner {
    this_replica_id: ReplicaId,
    message_brokers: Mutex<HashMap<ReplicaId, MessageBroker>>,
    to_forget: Mutable<HashSet<RuntimeId>>,
    task_handle: ScopedTaskHandle,
    index: Index,
}

impl Inner {
    async fn start(self: Arc<Self>, addr: SocketAddr) -> io::Result<()> {
        let listener = TcpListener::bind(addr).await?;

        self.task_handle.spawn(
            self.clone()
                .run_discovery(listener.local_addr().unwrap().port()),
        );
        self.task_handle.spawn(self.clone().run_listener(listener));

        Ok(())
    }

    async fn run_discovery(self: Arc<Self>, listener_port: u16) {
        let discovery =
            ReplicaDiscovery::new(listener_port).expect("Failed to create ReplicaDiscovery");

        loop {
            tokio::select! {
                found = discovery.wait_for_activity() => {

                    for (id, addr) in found {
                        let s = self.clone();
                        self.task_handle.spawn(async move {
                            let socket = match TcpStream::connect(addr).await {
                                Ok(socket) => socket,
                                Err(_) => return,
                            };
                            s.handle_new_connection(socket, Some(id)).await.ok();
                        });
                    }
                }
                _ = self.to_forget.changed() => {
                    let mut set = self.to_forget.value.lock().await;
                    for id in set.drain() {
                        discovery.forget(&id);
                    }
                }
            }
        }
    }

    async fn run_listener(self: Arc<Self>, listener: TcpListener) {
        loop {
            let (socket, _addr) = listener
                .accept()
                .await
                .expect("Failed to start TcpListener");

            let s = self.clone();
            self.task_handle.spawn(async move {
                s.handle_new_connection(socket, None).await.ok();
            });
        }
    }

    async fn handle_new_connection(
        self: Arc<Self>,
        socket: TcpStream,
        discovery_id: Option<RuntimeId>,
    ) -> io::Result<()> {
        let mut os = ObjectStream::new(socket);
        os.write(&self.this_replica_id).await?;
        let their_replica_id = os.read::<ReplicaId>().await?;

        let s = self.clone();
        let mut brokers = self.message_brokers.lock().await;
        let index = self.index.clone();

        let broker = brokers.entry(their_replica_id).or_insert_with(|| {
            MessageBroker::new(
                index,
                Box::pin(async move {
                    s.message_brokers.lock().await.remove(&their_replica_id);
                    if let Some(discovery_id) = discovery_id {
                        s.to_forget.modify(|set| set.insert(discovery_id)).await;
                    }
                }),
            )
        });

        broker.add_connection(os);

        Ok(())
    }
}

struct Mutable<T> {
    notify: tokio::sync::Notify,
    value: Mutex<T>,
}

impl<T> Mutable<T> {
    pub fn new(value: T) -> Mutable<T> {
        Mutable {
            notify: tokio::sync::Notify::new(),
            value: Mutex::new(value),
        }
    }

    pub async fn modify<F, R>(&self, f: F)
    where
        F: FnOnce(&mut T) -> R + Sized,
    {
        let mut v = self.value.lock().await;
        f(&mut v);
        self.notify.notify_one();
    }

    pub async fn changed(&self) {
        self.notify.notified().await
    }
}
