use std::collections::BTreeMap;

/// Small reorder buffer for RTP sequence numbers.
#[derive(Debug)]
pub struct JitterBuffer<T> {
    next: Option<u16>,
    packets: BTreeMap<u16, T>,
    capacity: usize,
    dropped: u64,
}

impl<T> JitterBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            next: None,
            packets: BTreeMap::new(),
            capacity: capacity.max(1),
            dropped: 0,
        }
    }

    #[allow(dead_code)]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Insert a packet and return any packets that are now in order.
    pub fn push(&mut self, sequence: u16, payload: T) -> Vec<(u16, T)> {
        if let Some(next) = self.next {
            if seq_less(sequence, next) {
                self.dropped += 1;
                return Vec::new();
            }
        }
        if self.packets.contains_key(&sequence) {
            self.dropped += 1;
            return Vec::new();
        }
        self.packets.insert(sequence, payload);
        if self.next.is_none() && self.packets.len() < 2 {
            return Vec::new();
        }
        if self.next.is_none() {
            self.next = self.packets.keys().next().copied();
        }
        if self.packets.len() > self.capacity {
            if let Some(oldest) = self.packets.keys().next().copied() {
                self.packets.remove(&oldest);
                self.dropped += 1;
                self.next = Some(oldest.wrapping_add(1));
            }
        }
        self.pop_ready()
    }

    fn pop_ready(&mut self) -> Vec<(u16, T)> {
        let mut ready = Vec::new();
        if self.next.is_none() {
            if let Some(first) = self.packets.keys().next().copied() {
                self.next = Some(first);
            }
        }
        while let Some(expected) = self.next {
            if let Some(payload) = self.packets.remove(&expected) {
                ready.push((expected, payload));
                self.next = Some(expected.wrapping_add(1));
                continue;
            }
            if let Some(newest) = self.packets.keys().next_back().copied() {
                if seq_delta(expected, newest) > (self.capacity as u16 / 2).max(2) {
                    self.dropped += 1;
                    self.next = Some(expected.wrapping_add(1));
                    continue;
                }
            }
            break;
        }
        ready
    }
}

fn seq_less(left: u16, right: u16) -> bool {
    left != right && left.wrapping_sub(right) > 32_768
}

fn seq_delta(from: u16, to: u16) -> u16 {
    to.wrapping_sub(from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorders_one_swap() {
        let mut buffer = JitterBuffer::new(8);
        assert!(buffer.push(2, b"b".to_vec()).is_empty());
        let ready = buffer.push(1, b"a".to_vec());
        let payloads: Vec<_> = ready.into_iter().map(|(_, payload)| payload).collect();
        assert_eq!(payloads, vec![b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn drops_late_packets() {
        let mut buffer = JitterBuffer::new(4);
        let _ = buffer.push(1, b"a".to_vec());
        let _ = buffer.push(1, b"late".to_vec());
        assert_eq!(buffer.dropped(), 1);
    }
}
