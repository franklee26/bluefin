use std::{os::fd::AsRawFd, sync::Arc, time::Duration};

use tokio::{
    fs::File,
    io::AsyncWriteExt,
    sync::{
        mpsc::{self, Receiver, Sender},
        RwLock,
    },
    time::sleep,
};

use crate::core::{error::BluefinError, serialisable::Serialisable};

use super::{stream_manager::WriteStreamManager, Result};

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum BufferedWriteStatus {
    SpawnReady,
    Spawned,
    Killed,
}

#[derive(Clone)]
pub(crate) struct BufferedWriteInner {
    pub(crate) status: BufferedWriteStatus,
    stream_id: u16,
    left: u64,
    src_conn_id: u32,
    dst_conn_id: u32,
}

impl BufferedWriteInner {
    pub fn new(stream_id: u16, left: u64, src_conn_id: u32, dst_conn_id: u32) -> Self {
        Self {
            stream_id,
            left,
            src_conn_id,
            dst_conn_id,
            status: BufferedWriteStatus::SpawnReady,
        }
    }
}

pub(crate) struct BufferedWrite {
    pub(crate) inner: Arc<RwLock<BufferedWriteInner>>,
    file: File,
    tx: Option<Sender<WriteCommand>>,
}
pub(crate) enum WriteResponse {
    Success { num_segments_written: usize },
    Error { error: BluefinError },
}

/// Channel used to send reponse back to the invoker.
pub(crate) type Responder<T> = tokio::sync::oneshot::Sender<T>;

pub(crate) enum WriteCommand {
    WriteAck {
        responder: Responder<WriteResponse>,
    },
    TryWriteData {
        data: Vec<u8>,
        responder: Responder<WriteResponse>,
    },
    Flush {
        responder: Responder<WriteResponse>,
    },
    Kill,
}

impl BufferedWrite {
    pub(crate) fn new(
        file: File,
        stream_id: u16,
        left: u64,
        src_conn_id: u32,
        dst_conn_id: u32,
    ) -> Self {
        BufferedWrite {
            file,
            inner: Arc::new(RwLock::new(BufferedWriteInner::new(
                stream_id,
                left,
                src_conn_id,
                dst_conn_id,
            ))),
            tx: None,
        }
    }

    /// Send a `WriteCommand` to the write buffer.
    pub(crate) async fn send(&self, cmd: WriteCommand) -> Result<()> {
        if let None = self.tx {
            return Err(BluefinError::WriteError("No tx available".to_string()));
        }

        if let Err(e) = self.tx.as_ref().unwrap().send(cmd).await {
            return Err(BluefinError::WriteError(format!(
                "Failed to issue command: {}",
                e
            )));
        }

        Ok(())
    }

    async fn try_write_data_impl(
        data: Vec<u8>,
        responder: Responder<WriteResponse>,
        manager: &mut WriteStreamManager,
        file: &mut File,
    ) {
        // Enqueue data
        manager.enqueue(data);

        // Try to get sendable segments
        // Nothing available
        if manager.data_to_segments() == 0 {
            let _ = responder.send(WriteResponse::Success {
                num_segments_written: 0,
            });
            return;
        }

        // Send out segments
        for seg in manager.segment_buffer_into_iter() {
            let bytes = seg.serialise();
            let _ = file.write_all(&bytes).await;
        }
    }

    async fn writer_job_impl(
        mut rx: Receiver<WriteCommand>,
        inner: Arc<RwLock<BufferedWriteInner>>,
        file: &mut File,
    ) {
        let mut manager = {
            let inner = inner.read().await;
            WriteStreamManager::new(
                inner.stream_id,
                inner.left,
                inner.src_conn_id,
                inner.dst_conn_id,
            )
        };

        while let Some(cmd) = rx.recv().await {
            match cmd {
                WriteCommand::Flush { responder } => {}
                WriteCommand::TryWriteData { data, responder } => {
                    BufferedWrite::try_write_data_impl(data, responder, &mut manager, file).await;
                }
                WriteCommand::WriteAck { responder } => {}
                WriteCommand::Kill => {
                    inner.write().await.status = BufferedWriteStatus::Killed;
                    return;
                }
            }

            sleep(Duration::from_millis(500)).await;
        }
    }

    pub(crate) async fn spawn_writer_job(&mut self) {
        match self.inner.read().await.status {
            BufferedWriteStatus::Killed | BufferedWriteStatus::Spawned => return,
            _ => {}
        }

        let (tx, rx) = mpsc::channel(10);

        let mut file = self.file.try_clone().await.unwrap();

        let inner = Arc::clone(&self.inner);

        tokio::spawn(async move {
            BufferedWrite::writer_job_impl(rx, inner, &mut file).await;
        });

        self.inner.write().await.status = BufferedWriteStatus::Spawned;

        self.tx = Some(tx);
    }
}
