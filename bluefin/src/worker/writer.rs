use std::{cmp::min, collections::VecDeque, sync::Arc};

use crate::core::Extract;
use crate::{
    core::header::{BluefinHeader, BluefinSecurityFields, PacketType},
    net::{MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM, MAX_BLUEFIN_PAYLOAD_SIZE_BYTES},
};
use bluefin_proto::error::BluefinError;
use bluefin_proto::BluefinResult;
use bytes::{Bytes, BytesMut};
use tokio::{
    net::UdpSocket,
    spawn,
    sync::mpsc::{self, Receiver, Sender, UnboundedReceiver, UnboundedSender},
};

/// Internal representation of an ack. These fields will be used to build a Bluefin ack packet.
#[derive(Clone, Copy, Default)]
struct AckData {
    base_packet_num: u64,
    num_packets_consumed: usize,
}

/// Bound for the data send channel. Must be large enough that the writer
/// pump can keep up under bursts (so callers rarely hit `WriteError`/await
/// backpressure), but small enough to put a hard cap on memory usage and
/// surface a slow consumer instead of accumulating arbitrary backlog.
///
/// At 4096 * 1500 B ≈ 6 MiB, this absorbs a fairly long stall while still
/// being well-bounded.
const DATA_CHANNEL_BOUND: usize = 4096;

/// [WriterHandler] is a handle for all write operations to the network. The writer handler is
/// responsible for accepting bytes, dividing them into Bluefin packets and then eventually sending
/// them on to the network. The handler ensures that the order in which the sends arrive at are
/// preserved and that we fit at most [MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM] bytes of bluefin packets
/// into a single UDP datagram.
#[derive(Clone)]
pub(crate) struct WriterHandler {
    socket: Arc<UdpSocket>,
    next_packet_num: u64,
    data_sender: Option<Sender<Bytes>>,
    ack_sender: Option<UnboundedSender<AckData>>,
    src_conn_id: u32,
    dst_conn_id: u32,
}

impl WriterHandler {
    pub(crate) fn new(
        socket: Arc<UdpSocket>,
        next_packet_num: u64,
        src_conn_id: u32,
        dst_conn_id: u32,
    ) -> Self {
        Self {
            socket,
            src_conn_id,
            dst_conn_id,
            next_packet_num,
            data_sender: None,
            ack_sender: None,
        }
    }

    pub(crate) fn start(&mut self) -> BluefinResult<()> {
        let (data_s, data_r) = mpsc::channel::<Bytes>(DATA_CHANNEL_BOUND);
        let (ack_s, ack_r) = mpsc::unbounded_channel();
        self.data_sender = Some(data_s);
        self.ack_sender = Some(ack_s);

        let next_packet_num = self.next_packet_num;
        let src_conn_id = self.src_conn_id;
        let dst_conn_id = self.dst_conn_id;
        let socket = Arc::clone(&self.socket);
        spawn(async move {
            Self::read_data(data_r, next_packet_num, src_conn_id, dst_conn_id, socket).await;
        });

        let socket = Arc::clone(&self.socket);
        spawn(async move {
            Self::read_ack(ack_r, socket, src_conn_id, dst_conn_id).await;
        });

        Ok(())
    }

    #[inline]
    async fn read_ack(
        mut rx: UnboundedReceiver<AckData>,
        socket: Arc<UdpSocket>,
        src_conn_id: u32,
        dst_conn_id: u32,
    ) {
        let mut ack_queue = VecDeque::with_capacity(64);
        let limit = 20;
        let mut b = Vec::with_capacity(limit);
        let mut pending_send: Option<Vec<u8>> = None;
        // Pre-allocated buffer pool for datagrams (eliminates allocation overhead)
        let mut datagram_pool: Vec<Vec<u8>> = (0..12)
            .map(|_| Vec::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM))
            .collect();
        loop {
            // Try to send pending data first
            if let Some(data) = pending_send.take() {
                if socket.try_send(&data).is_err() {
                    // Still can't send, wait for writable and continue
                    pending_send = Some(data);
                    let _ = socket.writable().await;
                    continue;
                }
            }

            b.clear();
            let size = rx.recv_many(&mut b, limit).await;
            if size == 0 {
                continue;
            }
            for i in 0..size {
                let _ = ack_queue.push_back(b[i]);
            }

            // Batch sends: send up to 12 datagrams per iteration
            // try_send will fail fast if not writable, no need to check first
            for i in 0..12 {
                datagram_pool[i].clear();
                if Self::consume_acks_into(
                    &mut ack_queue,
                    src_conn_id,
                    dst_conn_id,
                    &mut datagram_pool[i],
                ) {
                    if socket.try_send(&datagram_pool[i]).is_err() {
                        // Move buffer out to pending_send
                        pending_send = Some(std::mem::replace(
                            &mut datagram_pool[i],
                            Vec::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM),
                        ));
                        break;
                    }
                } else {
                    break;
                }
            }
        }
    }

    #[inline]
    async fn read_data(
        mut rx: Receiver<Bytes>,
        next_packet_num: u64,
        src_conn_id: u32,
        dst_conn_id: u32,
        socket: Arc<UdpSocket>,
    ) {
        let mut data_queue: VecDeque<Bytes> = VecDeque::with_capacity(64);
        let mut next_packet_num = next_packet_num;
        let limit = 20;
        // Pre-allocated buffer pool for datagrams (eliminates allocation overhead)
        let mut datagram_pool: Vec<Vec<u8>> = (0..12)
            .map(|_| Vec::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM))
            .collect();
        let mut b: Vec<Bytes> = Vec::with_capacity(limit);

        // Channel for parallel sends - decouple packetization from sending
        let (send_tx, mut send_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // Spawn dedicated sender task
        let socket_clone = Arc::clone(&socket);
        spawn(async move {
            while let Some(datagram) = send_rx.recv().await {
                // Keep retrying until sent
                loop {
                    match socket_clone.try_send(&datagram) {
                        Ok(_) => break,
                        Err(_) => {
                            let _ = socket_clone.writable().await;
                        }
                    }
                }
            }
        });

        loop {
            b.clear();
            let size = rx.recv_many(&mut b, limit).await;
            if size == 0 {
                continue;
            }
            // `Bytes` is cheap to move out of the Vec slot via `extract` (mem::replace
            // with `Bytes::default()`, which is an empty Bytes).
            for i in 0..size {
                let _ = data_queue.push_back(b[i].extract());
            }

            // Batch packetization: create up to 12 datagrams per iteration
            for i in 0..12 {
                datagram_pool[i].clear();
                if Self::consume_data_into(
                    &mut data_queue,
                    &mut next_packet_num,
                    src_conn_id,
                    dst_conn_id,
                    &mut datagram_pool[i],
                ) {
                    // Send asynchronously via channel - move ownership to avoid clone
                    let datagram = std::mem::replace(
                        &mut datagram_pool[i],
                        Vec::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM)
                    );
                    let _ = send_tx.send(datagram);
                } else {
                    break;
                }
            }
        }
    }

    /// Public send taking an owned [`Bytes`]. Zero-copy when the caller already
    /// holds a `Bytes` (e.g. from a buffer pool or `Bytes::clone()`); cheap
    /// regardless. Prefer this over [`Self::send_data`] on the hot path.
    ///
    /// Synchronous: returns [`BluefinError::WriteError`] (the channel-full case)
    /// instead of awaiting backpressure. Use [`Self::send_bytes_async`] if you
    /// want to block until the writer drains.
    #[inline]
    pub(crate) fn send_bytes(&self, payload: Bytes) -> BluefinResult<usize> {
        match self.data_sender {
            Some(ref sender) => {
                let len = payload.len();
                if sender.try_send(payload).is_err() {
                    return Err(BluefinError::WriteError(
                        "Failed to send data (channel full or closed)",
                    ));
                }
                Ok(len)
            }
            None => Err(BluefinError::WriteError("Sender is not available")),
        }
    }

    /// Async send taking an owned [`Bytes`]. Awaits backpressure when the
    /// channel is full (preferred for high-throughput producers; the `for`
    /// loop in the bench client uses this so it doesn't have to sleep at
    /// the end to let the writer task drain).
    #[inline]
    pub(crate) async fn send_bytes_async(&self, payload: Bytes) -> BluefinResult<usize> {
        match self.data_sender {
            Some(ref sender) => {
                let len = payload.len();
                if sender.send(payload).await.is_err() {
                    return Err(BluefinError::WriteError(
                        "Failed to send data (channel closed)",
                    ));
                }
                Ok(len)
            }
            None => Err(BluefinError::WriteError("Sender is not available")),
        }
    }

    #[inline]
    pub(crate) fn send_data(&self, payload: &[u8]) -> BluefinResult<usize> {
        match self.data_sender {
            Some(ref sender) => {
                // We don't own the caller's buffer, so this still allocates +
                // copies once. Callers who *do* own a `Bytes` should call
                // `send_bytes` instead and pay only a refcount bump.
                if sender.try_send(Bytes::copy_from_slice(payload)).is_err() {
                    return Err(BluefinError::WriteError(
                        "Failed to send data (channel full or closed)",
                    ));
                }
                Ok(payload.len())
            }
            None => Err(BluefinError::WriteError("Sender is not available")),
        }
    }

    #[inline]
    pub(crate) fn send_ack(
        &self,
        base_packet_num: u64,
        num_packets_consumed: usize,
    ) -> BluefinResult<()> {
        if self.ack_sender.is_none() {
            return Err(BluefinError::WriteError("Ack sender is not available"));
        }

        let data = AckData {
            base_packet_num,
            num_packets_consumed,
        };

        if self.ack_sender.as_ref().unwrap().send(data).is_err() {
            return Err(BluefinError::WriteError("Failed to send ack"));
        }
        Ok(())
    }

    /// Direct serialization: serialize header + payload into buffer without BluefinPacket allocation
    #[inline(always)]
    fn serialize_packet_direct(ans: &mut Vec<u8>, header: &BluefinHeader, payload: &[u8]) {
        let current_len = ans.len();
        let packet_len = 20 + payload.len();
        ans.reserve(packet_len);
        unsafe {
            ans.set_len(current_len + packet_len);
        }
        // Serialize header directly (20 bytes)
        header.serialise_into(&mut ans[current_len..current_len + 20]);
        // Copy payload directly
        ans[current_len + 20..current_len + packet_len].copy_from_slice(payload);
    }

    #[inline(always)]
    fn consume_data_into(
        queue: &mut VecDeque<Bytes>,
        next_packet_num: &mut u64,
        src_conn_id: u32,
        dst_conn_id: u32,
        ans: &mut Vec<u8>,
    ) -> bool {
        if queue.is_empty() {
            return false;
        }

        // `running_payload` is a `Bytes` so we can `split_to` chunks in O(1)
        // without ever copying or re-allocating. Only when we need to MERGE
        // multiple sends into one outgoing packet do we materialise into a
        // `BytesMut` (cold path; the bench never hits it).
        let mut running_payload: Bytes = queue.pop_front().unwrap();
        let mut bytes_remaining = MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM;
        // Create header once and reuse (only update packet_number and type_specific_payload)
        let mut header = BluefinHeader::new(
            src_conn_id,
            dst_conn_id,
            PacketType::UnencryptedData,
            0,
            BluefinSecurityFields::default(),
        );

        while !queue.is_empty() && bytes_remaining > 20 {
            if running_payload.len() >= bytes_remaining - 20 {
                while !running_payload.is_empty() && bytes_remaining >= 20 {
                    let max_bytes_to_take = min(
                        running_payload.len(),
                        min(MAX_BLUEFIN_PAYLOAD_SIZE_BYTES, bytes_remaining - 20),
                    );
                    header.with_packet_number(*next_packet_num);
                    header.type_specific_payload = max_bytes_to_take as u16;
                    // O(1) zero-copy slice of the front bytes; `running_payload`
                    // keeps the rest.
                    let chunk = running_payload.split_to(max_bytes_to_take);
                    Self::serialize_packet_direct(ans, &header, &chunk);
                    *next_packet_num += 1;
                    bytes_remaining -= max_bytes_to_take + 20;
                }

                if !running_payload.is_empty() {
                    queue.push_front(running_payload);
                }
                return !ans.is_empty();
            }

            if running_payload.len() >= MAX_BLUEFIN_PAYLOAD_SIZE_BYTES {
                let max_bytes_to_take = min(
                    running_payload.len(),
                    min(bytes_remaining - 20, MAX_BLUEFIN_PAYLOAD_SIZE_BYTES),
                );
                header.with_packet_number(*next_packet_num);
                header.type_specific_payload = max_bytes_to_take as u16;
                let chunk = running_payload.split_to(max_bytes_to_take);
                Self::serialize_packet_direct(ans, &header, &chunk);
                *next_packet_num += 1;
                bytes_remaining -= max_bytes_to_take + 20;
                continue;
            }

            // Cold path: we need to merge the next queued payload onto the tail
            // of `running_payload`. Allocate a `BytesMut` to hold the union.
            let data = queue.pop_front().unwrap();
            let potential_bytes_len = data.len();
            if potential_bytes_len + running_payload.len() > MAX_BLUEFIN_PAYLOAD_SIZE_BYTES {
                let mut merged =
                    BytesMut::with_capacity(running_payload.len() + data.len());
                merged.extend_from_slice(&running_payload);
                merged.extend_from_slice(&data);
                running_payload = merged.freeze();

                let max_bytes_to_take = min(
                    running_payload.len(),
                    min(MAX_BLUEFIN_PAYLOAD_SIZE_BYTES, bytes_remaining - 20),
                );
                header.with_packet_number(*next_packet_num);
                header.type_specific_payload = max_bytes_to_take as u16;
                let chunk = running_payload.split_to(max_bytes_to_take);
                Self::serialize_packet_direct(ans, &header, &chunk);
                *next_packet_num += 1;
                bytes_remaining -= max_bytes_to_take + 20;
            } else {
                // We can fit both the payload and the left over bytes
                let mut merged =
                    BytesMut::with_capacity(running_payload.len() + data.len());
                merged.extend_from_slice(&running_payload);
                merged.extend_from_slice(&data);
                running_payload = merged.freeze();
            }
        }

        while !running_payload.is_empty() && bytes_remaining >= 20 {
            let max_bytes_to_take = min(
                running_payload.len(),
                min(MAX_BLUEFIN_PAYLOAD_SIZE_BYTES, bytes_remaining - 20),
            );
            header.with_packet_number(*next_packet_num);
            header.type_specific_payload = max_bytes_to_take as u16;
            let chunk = running_payload.split_to(max_bytes_to_take);
            Self::serialize_packet_direct(ans, &header, &chunk);
            *next_packet_num += 1;
            bytes_remaining -= max_bytes_to_take + 20;
        }

        if !running_payload.is_empty() {
            queue.push_front(running_payload);
        }

        !ans.is_empty()
    }

    /// Legacy non-`*_into` packetizer kept around because the `#[cfg(test)]`
    /// suite exercises it directly. The hot path uses [`Self::consume_data_into`].
    #[allow(dead_code)]
    fn consume_data(
        queue: &mut VecDeque<Vec<u8>>,
        next_packet_num: &mut u64,
        src_conn_id: u32,
        dst_conn_id: u32,
    ) -> Option<Vec<u8>> {
        let mut ans = Vec::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);
        let mut bytes_remaining = MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM;
        // Pre-allocate extra capacity to prevent reallocation when merging payloads
        let mut running_payload = Vec::with_capacity(MAX_BLUEFIN_PAYLOAD_SIZE_BYTES * 2);

        let security_fields = BluefinSecurityFields::new(false, 0x0);
        // Reuse header struct - just update fields instead of creating new ones
        let mut header = BluefinHeader::new(
            src_conn_id,
            dst_conn_id,
            PacketType::UnencryptedData,
            0,
            security_fields,
        );

        while !queue.is_empty() && bytes_remaining > 20 {
            // We already have some bytes left over and it's more than we can afford. Take what
            // we can and end.
            if running_payload.len() >= bytes_remaining - 20 {
                // Keep taking as many bytes out of the running payload as we can afford to
                while !running_payload.is_empty() && bytes_remaining >= 20 {
                    let max_bytes_to_take = min(
                        running_payload.len(),
                        min(MAX_BLUEFIN_PAYLOAD_SIZE_BYTES, bytes_remaining - 20),
                    );
                    header.with_packet_number(*next_packet_num);
                    header.type_specific_payload = max_bytes_to_take as u16;
                    // Use split_off to avoid drain().collect() allocation
                    let remaining = running_payload.split_off(max_bytes_to_take);
                    let payload = std::mem::replace(&mut running_payload, remaining);
                    // Direct serialization - no BluefinPacket allocation
                    Self::serialize_packet_direct(&mut ans, &header, &payload);
                    *next_packet_num += 1;
                    bytes_remaining -= max_bytes_to_take + 20;
                }

                // Only push back if there are remaining bytes
                if !running_payload.is_empty() {
                    queue.push_front(running_payload);
                }
                return Some(ans);
            }

            // We just happen to have a completely full running payload. Let's take as much as we can.
            if running_payload.len() >= MAX_BLUEFIN_PAYLOAD_SIZE_BYTES {
                let max_bytes_to_take = min(
                    running_payload.len(),
                    min(bytes_remaining - 20, MAX_BLUEFIN_PAYLOAD_SIZE_BYTES),
                );
                header.with_packet_number(*next_packet_num);
                header.type_specific_payload = max_bytes_to_take as u16;
                // Use split_off to avoid drain().collect() allocation
                let remaining = running_payload.split_off(max_bytes_to_take);
                let payload = std::mem::replace(&mut running_payload, remaining);
                // Direct serialization - no BluefinPacket allocation
                Self::serialize_packet_direct(&mut ans, &header, &payload);
                *next_packet_num += 1;
                bytes_remaining -= max_bytes_to_take + 20;
                continue;
            }

            // We have room
            let data = queue.pop_front().unwrap();
            let potential_bytes_len = data.len();
            if potential_bytes_len + running_payload.len() > MAX_BLUEFIN_PAYLOAD_SIZE_BYTES {
                // We cannot simply fit both payloads into this packet.
                // Optimize extend with unsafe copy - eliminate bounds checks
                let old_len = running_payload.len();
                running_payload.reserve(data.len());
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        running_payload.as_mut_ptr().add(old_len),
                        data.len()
                    );
                    running_payload.set_len(old_len + data.len());
                }

                // Try to take as much as we can
                let max_bytes_to_take = min(
                    running_payload.len(),
                    min(MAX_BLUEFIN_PAYLOAD_SIZE_BYTES, bytes_remaining - 20),
                );
                header.with_packet_number(*next_packet_num);
                header.type_specific_payload = max_bytes_to_take as u16;
                // Use split_off to avoid drain().collect() allocation
                let remaining = running_payload.split_off(max_bytes_to_take);
                let payload = std::mem::replace(&mut running_payload, remaining);
                // Direct serialization - no BluefinPacket allocation
                Self::serialize_packet_direct(&mut ans, &header, &payload);
                *next_packet_num += 1;
                bytes_remaining -= max_bytes_to_take + 20;
            } else {
                // We can fit both the payload and the left over bytes
                // Optimize extend with unsafe copy - eliminate bounds checks
                let old_len = running_payload.len();
                running_payload.reserve(data.len());
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        running_payload.as_mut_ptr().add(old_len),
                        data.len()
                    );
                    running_payload.set_len(old_len + data.len());
                }
            }
        }

        // Take the remaining amount
        while !running_payload.is_empty() && bytes_remaining >= 20 {
            let max_bytes_to_take = min(
                running_payload.len(),
                min(MAX_BLUEFIN_PAYLOAD_SIZE_BYTES, bytes_remaining - 20),
            );
            header.with_packet_number(*next_packet_num);
            header.type_specific_payload = max_bytes_to_take as u16;
            // Truncate payload reusing buffer when possible
            let remaining = running_payload.split_off(max_bytes_to_take);
            let payload = std::mem::replace(&mut running_payload, remaining);
            // Direct serialization - no BluefinPacket allocation
            Self::serialize_packet_direct(&mut ans, &header, &payload);
            *next_packet_num += 1;
            bytes_remaining -= max_bytes_to_take + 20;
        }

        // Re-queue the remaining bytes
        if !running_payload.is_empty() {
            let _ = queue.push_front(running_payload);
        }

        if ans.is_empty() {
            return None;
        }
        Some(ans)
    }

    #[inline(always)]
    fn consume_acks_into(
        queue: &mut VecDeque<AckData>,
        src_conn_id: u32,
        dst_conn_id: u32,
        bytes: &mut Vec<u8>,
    ) -> bool {
        if queue.is_empty() {
            return false;
        }

        let mut header = BluefinHeader::new(
            src_conn_id,
            dst_conn_id,
            PacketType::Ack,
            0,
            BluefinSecurityFields::default(),
        );

        while !queue.is_empty() {
            if bytes.len() + 20 > MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM {
                break;
            }
            let ack_data = queue.pop_front().unwrap();
            header.with_packet_number(ack_data.base_packet_num);
            header.type_specific_payload = ack_data.num_packets_consumed as u16;
            let current_len = bytes.len();
            bytes.reserve(20);
            unsafe {
                bytes.set_len(current_len + 20);
            }
            header.serialise_into(&mut bytes[current_len..]);
        }

        !bytes.is_empty()
    }

    /// Legacy non-`*_into` ack packetizer kept around because the
    /// `#[cfg(test)]` suite exercises it directly. The hot path uses
    /// [`Self::consume_acks_into`].
    #[allow(dead_code)]
    fn consume_acks(
        queue: &mut VecDeque<AckData>,
        src_conn_id: u32,
        dst_conn_id: u32,
    ) -> Option<Vec<u8>> {
        let mut bytes = Vec::with_capacity(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);
        let security_fields = BluefinSecurityFields::new(false, 0x0);
        let mut header = BluefinHeader::new(
            src_conn_id,
            dst_conn_id,
            PacketType::Ack,
            0,
            security_fields,
        );
        while !queue.is_empty() && bytes.len() <= MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM {
            let data = queue.pop_front().unwrap();
            header.packet_number = data.base_packet_num;
            header.type_specific_payload = data.num_packets_consumed as u16;
            if bytes.len() + 20 <= MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM {
                let current_len = bytes.len();
                bytes.reserve(20);
                unsafe {
                    bytes.set_len(current_len + 20);
                }
                header.serialise_into(&mut bytes[current_len..]);
            } else {
                queue.push_front(data);
                break;
            }
        }

        if bytes.len() == 0 {
            return None;
        }

        Some(bytes)
    }
}

#[cfg(kani)]
mod verification_tests {
    use crate::worker::writer::WriterHandler;
    use std::collections::VecDeque;

    #[kani::proof]
    fn kani_writer_queue_consume_empty_data_behaves_as_expected() {
        let mut next_packet_num = kani::any();
        let mut queue = VecDeque::new();
        let prev = next_packet_num;
        assert!(WriterHandler::consume_data(
            &mut queue,
            &mut next_packet_num,
            kani::any(),
            kani::any()
        )
        .is_none());
        assert_eq!(next_packet_num, prev);
    }

    #[kani::proof]
    fn kani_writer_queue_consume_empty_ack_behaves_as_expected() {
        let mut queue = VecDeque::new();
        assert!(WriterHandler::consume_acks(&mut queue, kani::any(), kani::any()).is_none());
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use std::collections::VecDeque;

    use crate::worker::writer::{AckData, WriterHandler};
    use crate::{
        core::{header::PacketType, packet::BluefinPacket},
        net::{
            MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM, MAX_BLUEFIN_PACKETS_IN_UDP_DATAGRAM,
            MAX_BLUEFIN_PAYLOAD_SIZE_BYTES,
        },
    };

    #[rstest]
    #[case(550)]
    #[case(1)]
    #[case(10)]
    #[case(760)]
    fn writer_queue_consume_ack_for_one_datagram_behaves_as_expected(#[case] num_acks: usize) {
        let expected_byte_size = num_acks * 20;
        assert_ne!(expected_byte_size, 0);
        assert!(expected_byte_size <= MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);

        let mut queue = VecDeque::new();
        for _ in 0..num_acks {
            queue.push_back(AckData {
                base_packet_num: 1,
                num_packets_consumed: 3,
            });
        }

        let consume_res = WriterHandler::consume_acks(&mut queue, 0xbcd, 0x521);
        assert!(consume_res.is_some());

        let consume = consume_res.unwrap();
        assert_eq!(consume.len(), expected_byte_size);

        // Deserialise to get the packets.
        let packets_res = BluefinPacket::from_bytes(&consume);
        assert!(packets_res.is_ok());

        let packets = packets_res.unwrap();
        assert_eq!(packets.len(), num_acks);

        for p in packets {
            assert_eq!(p.len(), 20);
            assert_eq!(p.header.type_field, PacketType::Ack);
            assert_eq!(p.header.source_connection_id, 0xbcd);
            assert_eq!(p.header.destination_connection_id, 0x521);
            assert_eq!(p.header.type_specific_payload as usize, 3);
            assert_eq!(p.header.packet_number, 1);
        }

        // Because we are adding at most 1 datagram worth of acks, we get nothing more
        assert!(WriterHandler::consume_acks(&mut queue, 0x0, 0x0).is_none());
    }

    #[rstest]
    #[case(1000)]
    #[case(761)]
    #[case(1234)]
    #[case(763)]
    #[case(2000)]
    fn writer_queue_consume_ack_for_multiple_datagrams_behaves_as_expected(
        #[case] num_acks: usize,
    ) {
        let expected_byte_size = num_acks * 20;
        let num_datagrams = expected_byte_size.div_ceil(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);
        assert_ne!(expected_byte_size, 0);
        assert!(expected_byte_size > MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);
        assert!(num_datagrams > 1 && num_datagrams <= 10);

        let mut queue = VecDeque::new();
        for ix in 0..num_acks {
            queue.push_back(AckData {
                base_packet_num: ix as u64,
                num_packets_consumed: ix + 1,
            });
        }

        let consume_res = WriterHandler::consume_acks(&mut queue, 0xbcd, 0x521);
        assert!(consume_res.is_some());

        let consume = consume_res.unwrap();
        assert_eq!(consume.len(), MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);

        // Deserialise to get the packets.
        let packets_res = BluefinPacket::from_bytes(&consume);
        assert!(packets_res.is_ok());
        let packets = packets_res.unwrap();

        let mut p_num = 0;
        for (ix, p) in packets.iter().enumerate() {
            assert!(p.len() <= 20);
            assert_eq!(p.header.type_field, PacketType::Ack);
            assert_eq!(p.header.source_connection_id, 0xbcd);
            assert_eq!(p.header.destination_connection_id, 0x521);
            assert_eq!(p.header.packet_number, ix as u64);
            assert_eq!(p.header.type_specific_payload as usize, ix + 1);
            p_num = ix;
        }
        assert_ne!(p_num, 0);

        let mut actual_num_acks = 0;
        actual_num_acks += packets.len();

        let mut counter = 0;
        let mut consume_res = WriterHandler::consume_acks(&mut queue, 0x0, 0x0);
        while counter <= 10 && consume_res.is_some() {
            let consume = consume_res.unwrap();
            let packets_res = BluefinPacket::from_bytes(&consume);
            assert!(packets_res.is_ok());
            let packets = packets_res.unwrap();
            for (ix, p) in packets.iter().enumerate() {
                assert!(p.len() <= 20);
                assert_eq!(p.header.type_field, PacketType::Ack);
                assert_eq!(p.header.source_connection_id, 0x0);
                assert_eq!(p.header.destination_connection_id, 0x0);
                assert_eq!(p.header.packet_number, (ix + p_num + 1) as u64);
                assert_eq!(p.header.type_specific_payload as usize, ix + p_num + 2);
            }
            p_num += packets.len();

            actual_num_acks += packets.len();
            consume_res = WriterHandler::consume_acks(&mut queue, 0x0, 0x0);
            counter += 1;
        }
        assert_eq!(num_acks, actual_num_acks);
    }

    #[rstest]
    #[case(6, 550)]
    #[case(20, 700)]
    #[case(1, 10000)]
    #[case(2, 5000)]
    #[case(1, 15000)]
    #[case(1, 1)]
    #[case(10000, 1)]
    #[case(5555, 1)]
    #[case(5432, 2)]
    #[case(100, 100)]
    #[case(57, 57)]
    #[case(55, 56)]
    #[case(3, 2000)]
    #[case(10, 123)]
    fn writer_queue_consume_data_for_one_datagram_behaves_as_expected(
        #[case] num_iterations: usize,
        #[case] payload_size: usize,
    ) {
        let payload_size_total = num_iterations * payload_size;
        let num_packets_total = payload_size_total.div_ceil(MAX_BLUEFIN_PAYLOAD_SIZE_BYTES);
        let bytes_total = payload_size_total + (20 * num_packets_total);
        assert!(bytes_total <= MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);

        let mut queue = VecDeque::new();
        for ix in 0..num_iterations {
            let data = vec![ix as u8; payload_size];
            queue.push_back(data.to_vec());
        }

        let mut next_packet_num = 0;
        let src_conn_id = 0x123;
        let dst_conn_id = 0xabc;
        let consume_res =
            WriterHandler::consume_data(&mut queue, &mut next_packet_num, src_conn_id, dst_conn_id);
        assert!(consume_res.is_some());

        let consume = consume_res.unwrap();
        let bluefin_packets_created =
            (num_iterations * payload_size).div_ceil(MAX_BLUEFIN_PAYLOAD_SIZE_BYTES);
        assert_ne!(bluefin_packets_created, 0);
        // First next packet num was 0. If I created n packets then the last packet would have packet number n - 1.
        // Therefore, the next packet num is n - 1 + 1 = n;
        assert_eq!(next_packet_num, bluefin_packets_created as u64);
        assert_eq!(
            consume.len(),
            num_iterations * payload_size + (20 * bluefin_packets_created)
        );

        // Deserialise them to get the packets.
        let packets_res = BluefinPacket::from_bytes(&consume);
        assert!(packets_res.is_ok());

        let packets = packets_res.unwrap();
        assert_eq!(packets.len(), bluefin_packets_created);

        let mut payload_bytes = 0;
        for p in packets {
            assert!(p.len() <= MAX_BLUEFIN_PAYLOAD_SIZE_BYTES + 20);
            assert_eq!(p.header.type_field, PacketType::UnencryptedData);
            assert_eq!(p.header.source_connection_id, src_conn_id);
            assert_eq!(p.header.destination_connection_id, dst_conn_id);
            assert_eq!(p.header.type_specific_payload as usize, p.payload.len());
            payload_bytes += p.payload.len();
        }

        assert_eq!(payload_bytes, num_iterations * payload_size);

        // Since we added less than the max amount of bytes we can stuff in a datagram, one consume consumes
        // all of the data. There should be nothing left.
        assert!(consume.len() <= MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);
        assert!(
            WriterHandler::consume_data(&mut queue, &mut next_packet_num, 0x123, 0x456).is_none()
        );
    }

    #[rstest]
    #[case(30, 550)]
    #[case(1, 15200)]
    #[case(15200, 1)]
    #[case(577, 43)]
    #[case(2, 15200)]
    #[case(5, 15200)]
    #[case(1, 15001)]
    #[case(87, 292)]
    #[case(9, 15001)]
    #[case(1, 150000)]
    #[case(432, 234)]
    fn writer_queue_consume_data_for_multiple_datagram_behaves_as_expected(
        #[case] num_iterations: usize,
        #[case] payload_size: usize,
    ) {
        let payload_size_total = num_iterations * payload_size;
        let num_packets_total = payload_size_total.div_ceil(MAX_BLUEFIN_PAYLOAD_SIZE_BYTES);
        let bytes_total = payload_size_total + (20 * num_packets_total);
        assert!(bytes_total > MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);

        let num_datagrams = bytes_total.div_ceil(MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);
        assert!(num_datagrams >= 1 && num_datagrams <= 10);

        let mut expected_data = vec![];
        let mut queue = VecDeque::new();
        for ix in 0..num_iterations {
            let data = vec![ix as u8; payload_size];
            expected_data.extend_from_slice(&data);
            queue.push_back(data.to_vec());
        }

        let mut next_packet_num = 0;
        let src_conn_id = 0x123;
        let dst_conn_id = 0xabc;
        let consume_res =
            WriterHandler::consume_data(&mut queue, &mut next_packet_num, src_conn_id, dst_conn_id);
        assert!(consume_res.is_some());

        let consume = consume_res.unwrap();
        assert_eq!(consume.len(), MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);

        let packets_res = BluefinPacket::from_bytes(&consume);
        assert!(packets_res.is_ok());

        let packets = packets_res.unwrap();
        assert_eq!(packets.len(), MAX_BLUEFIN_PACKETS_IN_UDP_DATAGRAM);

        let mut actual_data = vec![];
        for p in packets {
            assert!(p.len() <= MAX_BLUEFIN_PAYLOAD_SIZE_BYTES + 20);
            assert_eq!(p.header.type_field, PacketType::UnencryptedData);
            assert_eq!(p.header.source_connection_id, src_conn_id);
            assert_eq!(p.header.destination_connection_id, dst_conn_id);
            assert_eq!(p.header.type_specific_payload as usize, p.payload.len());
            actual_data.extend_from_slice(&p.payload);
        }

        // Fetch the rest of the data. Our tests won't go beyond 10 datagrams worth of data
        // so we assert the count here just in case.
        let mut counter = 0;
        let mut consume_res =
            WriterHandler::consume_data(&mut queue, &mut next_packet_num, src_conn_id, dst_conn_id);
        while counter < 10 && consume_res.is_some() {
            let consume = consume_res.as_ref().unwrap();
            assert_ne!(consume.len(), 0);

            let packets_res = BluefinPacket::from_bytes(&consume);
            assert!(packets_res.is_ok());
            let packets = packets_res.unwrap();
            assert_ne!(packets.len(), 0);
            for p in packets {
                assert!(p.len() <= MAX_BLUEFIN_PAYLOAD_SIZE_BYTES + 20);
                assert_eq!(p.header.type_field, PacketType::UnencryptedData);
                assert_eq!(p.header.source_connection_id, src_conn_id);
                assert_eq!(p.header.destination_connection_id, dst_conn_id);
                assert_eq!(p.header.type_specific_payload as usize, p.payload.len());
                actual_data.extend_from_slice(&p.payload);
            }

            counter += 1;
            consume_res = WriterHandler::consume_data(
                &mut queue,
                &mut next_packet_num,
                src_conn_id,
                dst_conn_id,
            );
        }
        assert_eq!(num_datagrams, 1 + counter);
        assert_eq!(expected_data, actual_data);
    }
}
