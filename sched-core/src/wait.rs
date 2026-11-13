//! Fixed-capacity FIFO wait queue.
//!
//! The scheduler uses this for timer sleepers on each core. IPC uses the same
//! entity state machine but keeps its per-process rendezvous metadata in the
//! process table.

#[derive(Clone, Copy)]
pub struct WaitQueue<const N: usize> {
    entries: [u32; N],
    len: usize,
}

impl<const N: usize> Default for WaitQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> WaitQueue<N> {
    pub const fn new() -> Self {
        Self {
            entries: [0; N],
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn contains(&self, id: u32) -> bool {
        self.entries[..self.len].contains(&id)
    }

    pub fn push(&mut self, id: u32) -> Result<(), ()> {
        if self.len >= N {
            return Err(());
        }
        self.entries[self.len] = id;
        self.len += 1;
        Ok(())
    }

    pub fn pop_front(&mut self) -> Option<u32> {
        if self.len == 0 {
            return None;
        }
        let id = self.entries[0];
        for index in 1..self.len {
            self.entries[index - 1] = self.entries[index];
        }
        self.len -= 1;
        Some(id)
    }

    pub fn remove(&mut self, id: u32) -> bool {
        let Some(position) = self.entries[..self.len].iter().position(|entry| *entry == id)
        else {
            return false;
        };
        for index in position + 1..self.len {
            self.entries[index - 1] = self.entries[index];
        }
        self.len -= 1;
        true
    }

    pub fn for_each<F: FnMut(u32)>(&self, visitor: &mut F) {
        for id in &self.entries[..self.len] {
            visitor(*id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_order_and_capacity() {
        let mut queue = WaitQueue::<3>::new();
        queue.push(7).unwrap();
        queue.push(9).unwrap();
        queue.push(11).unwrap();
        assert_eq!(queue.push(13), Err(()));
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.pop_front(), Some(7));
        assert_eq!(queue.pop_front(), Some(9));
        assert_eq!(queue.pop_front(), Some(11));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn remove_keeps_order() {
        let mut queue = WaitQueue::<4>::new();
        for id in [1, 2, 3] {
            queue.push(id).unwrap();
        }
        assert!(queue.remove(2));
        assert!(!queue.remove(9));
        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.pop_front(), Some(3));
        assert_eq!(queue.pop_front(), None);
    }

    #[test]
    fn contains_and_clear() {
        let mut queue = WaitQueue::<2>::new();
        queue.push(4).unwrap();
        assert!(queue.contains(4));
        assert!(!queue.contains(5));
        queue.clear();
        assert!(queue.is_empty());
    }
}
