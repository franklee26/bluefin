use std::{char::MAX, collections::VecDeque};

use crate::core::error::BluefinError;

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

        if packet_number - self.smallest_expected_packet_number
            >= MAX_SLIDING_WINDOW_SIZE.try_into().unwrap()
        {
            return Err(BluefinError::BufferFullError);
        }

        // Find the index to insert into our sorted vector.
        let index = match self.ordered_packet_numbers.binary_search(&packet_number) {
            // Ok result means we have already stored this packet number before. Fail here.
            Ok(_) => return Err(BluefinError::UnexpectedPacketNumberError),
            // We have found the index to insert this number in. It is possible this index
            // is much to large for our vector.
            Err(index) => index,
        };

        self.ordered_packet_numbers.insert(index, packet_number);
        Ok(())
    }

    /// If present, returns the largest packet number that we have contiguously buffered. For example,
    /// if Some(10) were returned, that means we have accounted for all packet numbers 10 and below.
    /// We may have packet numbers larger than 10 but they are disjointed from the contiguous set.
    fn consume(&mut self) -> Option<u64> {
        // Nothing in the vector. Done.
        if self.ordered_packet_numbers.is_empty() {
            return None;
        }

        // There are entries in the vector but we are still missing the smallest expected
        // packet number. Nothing to consume.
        if self.ordered_packet_numbers[0] > self.smallest_expected_packet_number {
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
                // There is a jump. Reinsert the popped val in the front and return.
                self.ordered_packet_numbers.push_front(p_number);
                break;
            }
        }

        self.smallest_expected_packet_number = last_packet_number + 1;
        Some(last_packet_number)
    }
}

#[cfg(test)]
mod tests {
    use crate::{core::error::BluefinError, utils::window::MAX_SLIDING_WINDOW_SIZE};

    use super::SlidingWindow;

    #[test]
    fn sliding_window_behaves_as_expected() {
        // Start with a packet number of 100
        let mut sliding_window = SlidingWindow::new(100);
        // Nothing inserted, show return none
        assert_eq!(sliding_window.consume(), None);

        // We should fail if we insert a packet number less than 100
        let insert_res = sliding_window.insert_packet_number(99);
        assert!(insert_res.is_err());
        assert_eq!(
            insert_res.err().unwrap(),
            BluefinError::UnexpectedPacketNumberError
        );
        // Nothing inserted, should still fail
        assert_eq!(sliding_window.consume(), None);

        // Should be able to insert 101, 102, 103, 104 and 106
        assert_eq!(sliding_window.insert_packet_number(101), Ok(()));
        assert_eq!(sliding_window.insert_packet_number(102), Ok(()));
        assert_eq!(sliding_window.insert_packet_number(103), Ok(()));
        assert_eq!(sliding_window.insert_packet_number(104), Ok(()));
        assert_eq!(sliding_window.insert_packet_number(106), Ok(()));
        // Still nothing to consume since we are still missing packet #100
        assert_eq!(sliding_window.consume(), None);

        // Should not be able to insert above the window limit
        assert!(sliding_window
            .insert_packet_number(100 + u64::try_from(MAX_SLIDING_WINDOW_SIZE).unwrap())
            .is_err());

        // Complete a contiguous sequence [100, 104] inclusive.
        assert_eq!(sliding_window.insert_packet_number(100), Ok(()));
        let consume_res = sliding_window.consume();
        assert!(consume_res.is_some());
        assert_eq!(consume_res.unwrap(), 104);

        // Consuming again returns none since we are missing #105
        assert!(sliding_window.consume().is_none());

        // Insert #107 and #110
        assert_eq!(sliding_window.insert_packet_number(107), Ok(()));
        assert_eq!(sliding_window.insert_packet_number(110), Ok(()));
        assert!(sliding_window.consume().is_none());

        // Complete contiguous sequence [105, 107]
        assert_eq!(sliding_window.insert_packet_number(105), Ok(()));
        let consume_res = sliding_window.consume();
        assert!(consume_res.is_some());
        assert_eq!(consume_res.unwrap(), 107);
        assert!(sliding_window.consume().is_none());

        // Should not be able to insert above the window limit
        assert!(sliding_window
            .insert_packet_number(108 + u64::try_from(MAX_SLIDING_WINDOW_SIZE).unwrap())
            .is_err());
        assert!(sliding_window
            .insert_packet_number(107 + u64::try_from(MAX_SLIDING_WINDOW_SIZE).unwrap())
            .is_ok());
    }
}
