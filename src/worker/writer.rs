use std::{
    cmp::min,
    collections::VecDeque,
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
    time::Duration,
};

use tokio::{net::UdpSocket, time::sleep};

use crate::{
    core::{
        header::{BluefinHeader, BluefinSecurityFields, PacketType},
        packet::BluefinPacket,
        Serialisable,
    },
    net::{MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM, MAX_BLUEFIN_PAYLOAD_SIZE_BYTES},
    utils::common::BluefinResult,
};

/// Each writer queue holds a queue of `WriterQueueData`
enum WriterQueueData {
    Payload(Vec<u8>),
    Ack {
        base_packet_num: u64,
        num_packets_consumed: usize,
    },
}

pub(crate) struct WriterQueue {
    queue: VecDeque<WriterQueueData>,
    waker: Option<Waker>,
}

impl WriterQueue {
    pub(crate) fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            waker: None,
        }
    }

    #[inline]
    pub(crate) fn consume_data(
        &mut self,
        next_packet_num: &mut u64,
        src_conn_id: u32,
        dst_conn_id: u32,
    ) -> Option<Vec<u8>> {
        let mut ans = vec![];
        let mut bytes_remaining = MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM;
        let mut running_payload = vec![];

        let security_fields = BluefinSecurityFields::new(false, 0x0);
        let mut header = BluefinHeader::new(
            src_conn_id,
            dst_conn_id,
            PacketType::UnencryptedData,
            0,
            security_fields,
        );

        while !self.queue.is_empty() && bytes_remaining > 20 {
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
                    let p = BluefinPacket::builder()
                        .header(header)
                        .payload(running_payload[..max_bytes_to_take].to_vec())
                        .build();
                    ans.extend(p.serialise());
                    *next_packet_num += 1;
                    bytes_remaining -= max_bytes_to_take + 20;
                    running_payload = running_payload[max_bytes_to_take..].to_vec();
                }

                if !running_payload.is_empty() {
                    self.queue
                        .push_front(WriterQueueData::Payload(running_payload.to_vec()));
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
                let p = BluefinPacket::builder()
                    .header(header)
                    .payload(running_payload[..max_bytes_to_take].to_vec())
                    .build();
                ans.extend(p.serialise());
                *next_packet_num += 1;
                bytes_remaining -= max_bytes_to_take + 20;
                running_payload = running_payload[max_bytes_to_take..].to_vec();
                continue;
            }

            // We have room
            let data = self.queue.pop_front().unwrap();
            match data {
                WriterQueueData::Payload(p) => {
                    let potential_bytes_len = p.len();
                    if potential_bytes_len + running_payload.len() > MAX_BLUEFIN_PAYLOAD_SIZE_BYTES
                    {
                        // We cannot simply fit both payloads into this packet.
                        running_payload.extend(p);

                        // Try to take as much as we can
                        let max_bytes_to_take = min(
                            running_payload.len(),
                            min(MAX_BLUEFIN_PAYLOAD_SIZE_BYTES, bytes_remaining - 20),
                        );
                        header.with_packet_number(*next_packet_num);
                        header.type_specific_payload = max_bytes_to_take as u16;
                        let packet = BluefinPacket::builder()
                            .header(header)
                            .payload(running_payload[..max_bytes_to_take].to_vec())
                            .build();
                        ans.extend(packet.serialise());
                        *next_packet_num += 1;
                        bytes_remaining -= max_bytes_to_take + 20;
                        running_payload = running_payload[max_bytes_to_take..].to_vec();
                    } else {
                        // We can fit both the payload and the left over bytes
                        running_payload.extend(p);
                    }
                }
                _ => unreachable!(),
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
            let p = BluefinPacket::builder()
                .header(header)
                .payload(running_payload[..max_bytes_to_take].to_vec())
                .build();
            ans.extend(p.serialise());
            *next_packet_num += 1;
            running_payload = running_payload[max_bytes_to_take..].to_vec();
            bytes_remaining -= max_bytes_to_take + 20;
        }

        // Re-queue the remaining bytes
        if !running_payload.is_empty() {
            self.queue
                .push_front(WriterQueueData::Payload(running_payload));
        }

        if ans.is_empty() {
            return None;
        }
        Some(ans)
    }

    pub(crate) fn consume_acks(&mut self, src_conn_id: u32, dst_conn_id: u32) -> Option<Vec<u8>> {
        let mut bytes = vec![];
        let security_fields = BluefinSecurityFields::new(false, 0x0);
        let mut header = BluefinHeader::new(
            src_conn_id,
            dst_conn_id,
            PacketType::Ack,
            0,
            security_fields,
        );
        while !self.queue.is_empty() && bytes.len() <= MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM {
            let data = self.queue.pop_front().unwrap();
            match data {
                WriterQueueData::Ack {
                    base_packet_num: b,
                    num_packets_consumed: c,
                } => {
                    header.packet_number = b;
                    header.type_specific_payload = c as u16;
                    if bytes.len() + 20 <= MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM {
                        bytes.extend(header.serialise());
                    } else {
                        self.queue.push_front(data);
                        break;
                    }
                }
                _ => unreachable!(),
            }
        }

        if bytes.len() == 0 {
            return None;
        }

        Some(bytes)
    }
}

/// Queues write requests to be sent. Each connection can have one or more [WriterTxChannel].
#[derive(Clone)]
pub(crate) struct WriterTxChannel {
    data_queue: Arc<Mutex<WriterQueue>>,
    ack_queue: Arc<Mutex<WriterQueue>>,
    num_runs_without_sleep: u32,
}

impl WriterTxChannel {
    pub(crate) fn new(
        data_queue: Arc<Mutex<WriterQueue>>,
        ack_queue: Arc<Mutex<WriterQueue>>,
    ) -> Self {
        Self {
            data_queue,
            ack_queue,
            num_runs_without_sleep: 0,
        }
    }

    /// ONLY for sending data
    pub(crate) async fn send(&mut self, payload: &[u8]) -> BluefinResult<usize> {
        let bytes = payload.len();
        let data = WriterQueueData::Payload(payload.to_vec());

        {
            let mut guard = self.data_queue.lock().unwrap();
            guard.queue.push_back(data);

            // Signal to Rx channel that we have new packets in the queue
            if let Some(ref waker) = guard.waker {
                waker.wake_by_ref();
            }
        }

        self.num_runs_without_sleep += 1;
        if self.num_runs_without_sleep >= 100 {
            sleep(Duration::from_nanos(10)).await;
            self.num_runs_without_sleep = 0;
        }

        Ok(bytes)
    }

    pub(crate) async fn send_ack(
        &mut self,
        base_packet_num: u64,
        num_packets_consumed: usize,
    ) -> BluefinResult<()> {
        let data = WriterQueueData::Ack {
            base_packet_num,
            num_packets_consumed,
        };

        {
            let mut guard = self.ack_queue.lock().unwrap();
            guard.queue.push_back(data);

            // Signal to Rx channel that we have new packets in the queue
            if let Some(ref waker) = guard.waker {
                waker.wake_by_ref();
            }
        }

        Ok(())
    }
}

#[derive(Clone)]
struct WriterRxChannelDataFuture {
    data_queue: Arc<Mutex<WriterQueue>>,
}

#[derive(Clone)]
struct WriterRxChannelAckFuture {
    ack_queue: Arc<Mutex<WriterQueue>>,
}

/// Consumes queued requests and sends them across the wire. For now, each connection
/// has one and only one [WriterRxChannel]. This channel must run two separate jobs:
/// [WriterRxChannel::run_data], which reads out of the data queue and sends bluefin
/// packets w/ payloads across the wire AND [WriterRxChannel::run_ack], which reads
/// acks out of the ack queue and sends bluefin ack packets across the wire.
#[derive(Clone)]
pub(crate) struct WriterRxChannel {
    data_future: WriterRxChannelDataFuture,
    ack_future: WriterRxChannelAckFuture,
    dst_addr: SocketAddr,
    next_packet_num: u64,
    src_conn_id: u32,
    dst_conn_id: u32,
    socket: Arc<UdpSocket>,
}

impl Future for WriterRxChannelDataFuture {
    type Output = usize;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut guard = self.data_queue.lock().unwrap();
        let num_packets_to_send = guard.queue.len();
        if num_packets_to_send == 0 {
            guard.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        Poll::Ready(num_packets_to_send)
    }
}

impl Future for WriterRxChannelAckFuture {
    type Output = usize;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut guard = self.ack_queue.lock().unwrap();
        let num_packets_to_send = guard.queue.len();
        if num_packets_to_send == 0 {
            guard.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        Poll::Ready(num_packets_to_send)
    }
}

impl WriterRxChannel {
    pub(crate) fn new(
        data_queue: Arc<Mutex<WriterQueue>>,
        ack_queue: Arc<Mutex<WriterQueue>>,
        socket: Arc<UdpSocket>,
        dst_addr: SocketAddr,
        next_packet_num: u64,
        src_conn_id: u32,
        dst_conn_id: u32,
    ) -> Self {
        let data_future = WriterRxChannelDataFuture {
            data_queue: Arc::clone(&data_queue),
        };
        let ack_future = WriterRxChannelAckFuture {
            ack_queue: Arc::clone(&ack_queue),
        };
        Self {
            data_future,
            ack_future,
            dst_addr,
            next_packet_num,
            src_conn_id,
            dst_conn_id,
            socket: Arc::clone(&socket),
        }
    }
    pub(crate) async fn run_ack(&mut self) {
        loop {
            let _ = self.ack_future.clone().await;
            let mut guard = self.ack_future.ack_queue.lock().unwrap();
            let bytes = guard.consume_acks(self.src_conn_id, self.dst_conn_id);
            match bytes {
                None => continue,
                Some(b) => {
                    if let Err(e) = self.socket.try_send_to(&b, self.dst_addr) {
                        eprintln!(
                            "Encountered error {} while sending ack packet across wire",
                            e
                        );
                        continue;
                    }
                }
            }
            guard.waker = None;
        }
    }

    pub(crate) async fn run_data(&mut self) {
        loop {
            let _ = self.data_future.clone().await;
            let mut guard = self.data_future.data_queue.lock().unwrap();
            let bytes = guard.consume_data(
                &mut self.next_packet_num,
                self.src_conn_id,
                self.dst_conn_id,
            );
            match bytes {
                None => continue,
                Some(b) => {
                    if let Err(e) = self.socket.try_send_to(&b, self.dst_addr) {
                        eprintln!(
                            "Encountered error {} while sending data packet across wire",
                            e
                        );
                        continue;
                    }
                }
            }
            guard.waker = None;
        }
    }
}

#[cfg(kani)]
mod verification_tests {
    use crate::worker::writer::WriterQueue;

    #[kani::proof]
    fn kani_writer_queue_consume_empty_data_behaves_as_expected() {
        let mut writer_q = WriterQueue::new();
        let mut next_packet_num = kani::any();
        let prev = next_packet_num;
        assert!(writer_q
            .consume_data(&mut next_packet_num, kani::any(), kani::any())
            .is_none());
        assert_eq!(next_packet_num, prev);
    }

    #[kani::proof]
    fn kani_writer_queue_consume_empty_ack_behaves_as_expected() {
        let mut writer_q = WriterQueue::new();
        assert!(writer_q.consume_acks(kani::any(), kani::any()).is_none());
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use crate::{
        core::{header::PacketType, packet::BluefinPacket},
        net::{
            MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM, MAX_BLUEFIN_PACKETS_IN_UDP_DATAGRAM,
            MAX_BLUEFIN_PAYLOAD_SIZE_BYTES,
        },
        worker::writer::WriterQueue,
    };

    use super::WriterQueueData;

    #[rstest]
    #[test]
    #[case(550)]
    #[case(1)]
    #[case(10)]
    #[case(760)]
    fn writer_queue_consume_ack_for_one_datagram_behaves_as_expected(#[case] num_acks: usize) {
        let expected_byte_size = num_acks * 20;
        assert_ne!(expected_byte_size, 0);
        assert!(expected_byte_size <= MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);

        let mut writer_q = WriterQueue::new();
        for _ in 0..num_acks {
            writer_q.queue.push_back(WriterQueueData::Ack {
                base_packet_num: 1,
                num_packets_consumed: 3,
            });
        }

        let consume_res = writer_q.consume_acks(0xbcd, 0x521);
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
        assert!(writer_q.consume_acks(0x0, 0x0).is_none());
    }

    #[rstest]
    #[test]
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

        let mut writer_q = WriterQueue::new();
        for ix in 0..num_acks {
            writer_q.queue.push_back(WriterQueueData::Ack {
                base_packet_num: ix as u64,
                num_packets_consumed: ix + 1,
            });
        }

        let consume_res = writer_q.consume_acks(0xbcd, 0x521);
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
        assert!(p_num != 0);

        let mut actual_num_acks = 0;
        actual_num_acks += packets.len();

        let mut counter = 0;
        let mut consume_res = writer_q.consume_acks(0x0, 0x0);
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
            consume_res = writer_q.consume_acks(0x0, 0x0);
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
    #[test]
    fn writer_queue_consume_data_for_one_datagram_behaves_as_expected(
        #[case] num_iterations: usize,
        #[case] payload_size: usize,
    ) {
        let payload_size_total = num_iterations * payload_size;
        let num_packets_total = payload_size_total.div_ceil(MAX_BLUEFIN_PAYLOAD_SIZE_BYTES);
        let bytes_total = payload_size_total + (20 * num_packets_total);
        assert!(bytes_total <= MAX_BLUEFIN_BYTES_IN_UDP_DATAGRAM);

        let mut writer_q = WriterQueue::new();
        for ix in 0..num_iterations {
            let data = vec![ix as u8; payload_size];
            writer_q
                .queue
                .push_back(WriterQueueData::Payload(data.to_vec()));
        }

        let mut next_packet_num = 0;
        let src_conn_id = 0x123;
        let dst_conn_id = 0xabc;
        let consume_res = writer_q.consume_data(&mut next_packet_num, src_conn_id, dst_conn_id);
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
        assert!(writer_q
            .consume_data(&mut next_packet_num, 0x123, 0x456)
            .is_none());
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
    #[test]
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
        let mut writer_q = WriterQueue::new();
        for ix in 0..num_iterations {
            let data = vec![ix as u8; payload_size];
            expected_data.extend_from_slice(&data);
            writer_q
                .queue
                .push_back(WriterQueueData::Payload(data.to_vec()));
        }

        let mut next_packet_num = 0;
        let src_conn_id = 0x123;
        let dst_conn_id = 0xabc;
        let consume_res = writer_q.consume_data(&mut next_packet_num, src_conn_id, dst_conn_id);
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
        let mut consume_res = writer_q.consume_data(&mut next_packet_num, src_conn_id, dst_conn_id);
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
            consume_res = writer_q.consume_data(&mut next_packet_num, src_conn_id, dst_conn_id);
        }
        assert_eq!(num_datagrams, 1 + counter);
        assert_eq!(expected_data, actual_data);
    }
}
