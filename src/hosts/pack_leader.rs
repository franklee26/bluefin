use std::{os::fd::FromRawFd, sync::Arc};

use crate::{
    core::context::{BluefinHost, State},
    handshake::handshake::HandshakeHandler,
    io::{
        buffered_read::BufferedRead,
        manager::{ConnectionManager, Result},
        worker::{AcceptWorker, ReadWorker},
    },
    network::connection::Connection,
    tun::device::BluefinDevice,
    utils::common::build_connection_from_packet,
};
use rand::Rng;
use tokio::fs::File;

const NUMBER_OF_WORKER_THREADS: usize = 2;

pub struct BluefinPackLeader {
    file: File,
    manager: Arc<tokio::sync::Mutex<ConnectionManager>>,
    spawned_read_thread: bool,
}

impl BluefinPackLeader {
    pub fn builder() -> BluefinPackLeaderBuilder {
        BluefinPackLeaderBuilder {
            name: None,
            bind_address: None,
            netmask: None,
        }
    }
}

pub struct BluefinPackLeaderBuilder {
    name: Option<String>,
    bind_address: Option<String>,
    netmask: Option<String>,
}

impl BluefinPackLeaderBuilder {
    pub fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    pub fn bind_address(mut self, bind_address: String) -> Self {
        self.bind_address = Some(bind_address);
        self
    }

    pub fn netmask(mut self, netmask: String) -> Self {
        self.netmask = Some(netmask);
        self
    }

    pub fn build(&self) -> BluefinPackLeader {
        let name = self.name.clone().unwrap_or("utun3".to_string());
        let address = self.bind_address.clone().unwrap();
        let netmask = self.netmask.clone().unwrap();

        let device = BluefinDevice::builder()
            .name(name.to_string())
            .address(address.parse().unwrap())
            .netmask(netmask.parse().unwrap())
            .build();

        let fd = device.get_raw_fd();
        let file = unsafe { File::from_raw_fd(fd) };

        let manager = ConnectionManager::new();

        BluefinPackLeader {
            file,
            manager: Arc::new(tokio::sync::Mutex::new(manager)),
            spawned_read_thread: false,
        }
    }
}

impl BluefinPackLeader {
    /// Spawns some reader-worker threads. These threads will only get spawned at first
    /// invocation of `accept` and will run forever until the main thread has completed.
    #[inline]
    async fn spawn_read_threads(&mut self) {
        if self.spawned_read_thread {
            return;
        }

        for _ in 0..NUMBER_OF_WORKER_THREADS {
            let mut worker = ReadWorker::new(
                Arc::clone(&self.manager),
                self.file.try_clone().await.unwrap(),
                true,
                BluefinHost::PackLeader,
            );

            tokio::spawn(async move {
                worker.run().await;
            });
        }

        self.spawned_read_thread = true;
    }

    /// Spawns a single `Accept`-helper worker. These workers are spawned once per `accept()` invocation
    /// and have a finite lifetime; they check if there are any potential matches between an existing
    /// unhandled client-hello and an pending `Accept` request.
    #[inline]
    fn spawn_accept_read_threads(&self) -> tokio::task::JoinHandle<()> {
        // Spawn an Accept helper
        let accept_worker = AcceptWorker::new(Arc::clone(&self.manager));
        tokio::spawn(async move {
            accept_worker.run().await;
        })
    }

    /// Pack-leader accepts a bluefin connection request. Upon first invocation, this function spawns
    /// a number of read thread workers. This function waits asynchronously for a client hello, registers
    /// the relevant connection buffers and completes the handshake.
    pub async fn accept(&mut self) -> Result<Connection> {
        // Spawn some read worker threads
        self.spawn_read_threads().await;

        // Generate source id
        let source_id: u32 = rand::thread_rng().gen();
        let temp_key = format!("0_{}", source_id);

        // Lock acquired
        let buffer = {
            let mut manager = self.manager.lock().await;
            manager.register_new_accept_connection(&temp_key)
        };
        // Lock released

        // Spawn accept-helper worker
        let accept_worker_join_handle = self.spawn_accept_read_threads();

        // Await for client-hello
        let packet = BufferedRead::new(Arc::clone(&buffer)).await;

        // Abort the accept worker thread, we already found a packet!
        accept_worker_join_handle.abort();

        let mut conn = build_connection_from_packet(
            &packet,
            source_id,
            BluefinHost::PackLeader,
            true,
            Arc::clone(&buffer),
            self.file.try_clone().await.unwrap(),
        );
        let key = format!("{}_{}", conn.dest_id, conn.source_id);

        // Lock acquired
        {
            let mut manager = self.manager.lock().await;
            // generate new entry (no need to delete old entry, handled by worker job)
            manager.register_new_connection(&key, Arc::clone(&buffer));
        }
        // Lock released

        let mut handshake_handler =
            HandshakeHandler::new(&mut conn, packet, BluefinHost::PackLeader);
        handshake_handler.handle().await?;

        conn.context.state = State::Ready;

        Ok(conn)
    }
}
