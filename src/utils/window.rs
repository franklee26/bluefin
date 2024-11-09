use std::collections::VecDeque;

use crate::core::{error::BluefinError, packet};

use super::common::BluefinResult;

pub const MAX_SLIDING_WINDOW_SIZE: usize = 10;

#[derive(Clone)]
pub(crate) struct SlidingWindow {
    smallest_expected_packet_number: u64,
    ordered_packet_numbers: VecDeque<u64>,
}

impl SlidingWindow {
    pub(crate) fn new(smallest_expected_packet_number: u64) -> Self {
        Self {
            smallest_expected_packet_number,
            ordered_packet_numbers: VecDeque::new(),
        }
    }

    pub(crate) fn insert_packet_number(&mut self, packet_number: u64) -> BluefinResult<()> {
        // Impossible, we must have already considered this packet.
        if packet_number < self.smallest_expected_packet_number {
            return Err(BluefinError::UnexpectedPacketNumberError);
        }

        // Find the index to insert into our sorted vector.
        let index = match self.ordered_packet_numbers.binary_search(&packet_number) {
            // Ok result means we have already stored this packet number before. Fail here.
            Ok(_) => return Err(BluefinError::UnexpectedPacketNumberError),
            // We have found the index to insert this number in. It is possible this index
            // is much to large for our vector.
            Err(index) => index,
        };

        // We cannot accomodate this packet number. This means thie packet number is so high
        // that we would have to allocate too much memory.
        if index >= MAX_SLIDING_WINDOW_SIZE {
            return Err(BluefinError::BufferFullError);
        }

        self.ordered_packet_numbers.insert(index, packet_number);
        self.smallest_expected_packet_number = self.ordered_packet_numbers[0];
        Ok(())
    }

    /// If present, returns the largest packet number that we have contigously buffered. For example,
    /// if Some(10) were returned, that means we have accounted for all packet numbers 10 and below.
    /// We may have packet numbers larger than 10 but they are disjointed from the contiguous set.
    fn consume(&mut self) -> Option<u64> {
        // Nothing in the vector. Done.
        if self.ordered_packet_numbers.is_empty() {
            return None;
        }

        // Vector is not empty so this is safe.
        let mut last_packet_number = self.ordered_packet_numbers.pop_front().unwrap();
        while !self.ordered_packet_numbers.is_empty() {
            let p_number = self.ordered_packet_numbers.pop_front().unwrap();

            if p_number == last_packet_number + 1 {
                // contiguous. Keep going.
                last_packet_number = p_number;
                continue;
            } else {
                // There is a jump. Reinsert the poped val in the front and return.
                self.ordered_packet_numbers.push_front(p_number);
                break;
            }
        }

        Some(last_packet_number)
    }
}
