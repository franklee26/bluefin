use rand::Rng;

use crate::utils::common::SyncConnBufferRef;

pub enum StreamType {
    Bidirectional,
    SendOnly,
    ReadOnly,
}

pub struct Stream {
    pub id: u32,
    pub conn_id: u32,
    pub stream_type: StreamType,
    pub(crate) buffer: SyncConnBufferRef,
}

impl Stream {
    pub(crate) fn new(conn_id: u32, stream_type: StreamType, buffer: SyncConnBufferRef) -> Self {
        let id: u32 = rand::thread_rng().gen();
        Self {
            id,
            conn_id,
            stream_type,
            buffer,
        }
    }
}
