//! Fixed-capacity indexed AVL tree for run queues.
//!
//! The tree stores a [`Key`] per node. Keys are ordered by virtual deadline and
//! then by entity id, which gives a total order even when two entities share a
//! deadline. Nodes live in a fixed array; the tree never allocates and never
//! moves an entity's storage. The kernel keeps the entity itself in its own
//! table and uses [`Key::id`] to find it.
//!
//! Parent links make insertion, removal and rebalancing iterative. `validate`
//! is intended for the debug-only scheduler invariant checker and host tests.

/// Sentinel index for "no node".
pub const NONE: u16 = u16::MAX;

/// Run-queue key: earliest virtual deadline first, entity id as tie-breaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub deadline: u64,
    pub id: u32,
}

impl Key {
    pub const fn new(deadline: u64, id: u32) -> Self {
        Self { deadline, id }
    }

    pub const ZERO: Self = Self {
        deadline: 0,
        id: 0,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeError {
    /// The fixed node array is exhausted.
    Full,
    /// An equal key is already present.
    Duplicate,
    /// The key is not present.
    NotFound,
    /// An internal invariant is broken.
    Invalid,
}

#[derive(Clone, Copy)]
struct Node {
    deadline: u64,
    id: u32,
    left: u16,
    right: u16,
    parent: u16,
    height: u8,
    used: bool,
}

impl Node {
    const EMPTY: Self = Self {
        deadline: 0,
        id: 0,
        left: NONE,
        right: NONE,
        parent: NONE,
        height: 0,
        used: false,
    };
}

/// An indexed AVL tree with `N` fixed node slots.
pub struct RunTree<const N: usize> {
    nodes: [Node; N],
    root: u16,
    free: u16,
    len: usize,
}

impl<const N: usize> Default for RunTree<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> RunTree<N> {
    /// Build an empty tree and thread the free list through `left`.
    pub const fn new() -> Self {
        let mut nodes = [Node::EMPTY; N];
        let mut index = 0usize;
        while index < N {
            nodes[index].left = if index + 1 < N {
                (index + 1) as u16
            } else {
                NONE
            };
            index += 1;
        }
        Self {
            nodes,
            root: NONE,
            free: if N == 0 { NONE } else { 0 },
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        N
    }

    pub fn clear(&mut self) {
        *self = Self::new();
    }

    fn key(&self, index: u16) -> Key {
        let node = &self.nodes[index as usize];
        Key::new(node.deadline, node.id)
    }

    fn height(&self, index: u16) -> u8 {
        if index == NONE {
            0
        } else {
            self.nodes[index as usize].height
        }
    }

    fn update_height(&mut self, index: u16) {
        let left = self.height(self.nodes[index as usize].left);
        let right = self.height(self.nodes[index as usize].right);
        self.nodes[index as usize].height = 1 + if left > right { left } else { right };
    }

    fn balance(&self, index: u16) -> i16 {
        self.height(self.nodes[index as usize].left) as i16
            - self.height(self.nodes[index as usize].right) as i16
    }

    fn take_free(&mut self) -> Option<u16> {
        if self.free == NONE {
            return None;
        }
        let index = self.free;
        self.free = self.nodes[index as usize].left;
        Some(index)
    }

    fn release(&mut self, index: u16) {
        let node = &mut self.nodes[index as usize];
        node.used = false;
        node.height = 0;
        node.right = NONE;
        node.parent = NONE;
        node.left = self.free;
        self.free = index;
    }

    fn find(&self, key: Key) -> u16 {
        let mut cursor = self.root;
        while cursor != NONE {
            let node = &self.nodes[cursor as usize];
            let node_key = Key::new(node.deadline, node.id);
            cursor = match key.cmp(&node_key) {
                core::cmp::Ordering::Less => node.left,
                core::cmp::Ordering::Greater => node.right,
                core::cmp::Ordering::Equal => return cursor,
            };
        }
        NONE
    }

    pub fn contains(&self, key: Key) -> bool {
        self.find(key) != NONE
    }

    pub fn insert(&mut self, key: Key) -> Result<(), TreeError> {
        if self.len >= N {
            return Err(TreeError::Full);
        }

        let mut parent = NONE;
        let mut cursor = self.root;
        while cursor != NONE {
            let node = &self.nodes[cursor as usize];
            let node_key = Key::new(node.deadline, node.id);
            parent = cursor;
            cursor = match key.cmp(&node_key) {
                core::cmp::Ordering::Less => node.left,
                core::cmp::Ordering::Greater => node.right,
                core::cmp::Ordering::Equal => return Err(TreeError::Duplicate),
            };
        }

        let index = self.take_free().ok_or(TreeError::Full)?;
        {
            let node = &mut self.nodes[index as usize];
            node.deadline = key.deadline;
            node.id = key.id;
            node.left = NONE;
            node.right = NONE;
            node.parent = parent;
            node.height = 1;
            node.used = true;
        }

        if parent == NONE {
            self.root = index;
        } else if key < self.key(parent) {
            self.nodes[parent as usize].left = index;
        } else {
            self.nodes[parent as usize].right = index;
        }

        self.len += 1;
        self.rebalance_from(parent);
        Ok(())
    }

    pub fn remove(&mut self, key: Key) -> Result<(), TreeError> {
        let mut target = self.find(key);
        if target == NONE {
            return Err(TreeError::NotFound);
        }

        if self.nodes[target as usize].left != NONE && self.nodes[target as usize].right != NONE {
            let successor = self.min_node(self.nodes[target as usize].right);
            if successor == NONE {
                return Err(TreeError::Invalid);
            }
            self.nodes[target as usize].deadline = self.nodes[successor as usize].deadline;
            self.nodes[target as usize].id = self.nodes[successor as usize].id;
            target = successor;
        }

        let child = if self.nodes[target as usize].left != NONE {
            self.nodes[target as usize].left
        } else {
            self.nodes[target as usize].right
        };
        let parent = self.nodes[target as usize].parent;

        if child != NONE {
            self.nodes[child as usize].parent = parent;
        }
        if parent == NONE {
            self.root = child;
        } else if self.nodes[parent as usize].left == target {
            self.nodes[parent as usize].left = child;
        } else {
            self.nodes[parent as usize].right = child;
        }

        self.release(target);
        self.len -= 1;

        if parent != NONE {
            self.rebalance_from(parent);
        } else if child != NONE {
            self.update_height(child);
        }
        Ok(())
    }

    pub fn first(&self) -> Option<Key> {
        let index = self.min_node(self.root);
        if index == NONE {
            None
        } else {
            Some(self.key(index))
        }
    }

    pub fn last(&self) -> Option<Key> {
        let index = self.max_node(self.root);
        if index == NONE {
            None
        } else {
            Some(self.key(index))
        }
    }

    pub fn remove_first(&mut self) -> Option<Key> {
        let key = self.first()?;
        let _ = self.remove(key);
        Some(key)
    }

    pub fn remove_last(&mut self) -> Option<Key> {
        let key = self.last()?;
        let _ = self.remove(key);
        Some(key)
    }

    /// First key whose deadline is at least `deadline`.
    pub fn first_at_or_after(&self, deadline: u64) -> Option<Key> {
        self.first_ge(self.root, deadline)
    }

    fn first_ge(&self, index: u16, deadline: u64) -> Option<Key> {
        if index == NONE {
            return None;
        }
        let node = &self.nodes[index as usize];
        if node.deadline < deadline {
            return self.first_ge(node.right, deadline);
        }
        if let Some(found) = self.first_ge(node.left, deadline) {
            return Some(found);
        }
        Some(Key::new(node.deadline, node.id))
    }

    fn min_node(&self, mut index: u16) -> u16 {
        if index == NONE {
            return NONE;
        }
        loop {
            let left = self.nodes[index as usize].left;
            if left == NONE {
                return index;
            }
            index = left;
        }
    }

    fn max_node(&self, mut index: u16) -> u16 {
        if index == NONE {
            return NONE;
        }
        loop {
            let right = self.nodes[index as usize].right;
            if right == NONE {
                return index;
            }
            index = right;
        }
    }

    /// Visit every key in ascending order.
    pub fn for_each<F: FnMut(Key)>(&self, visitor: &mut F) {
        self.visit(self.root, visitor);
    }

    fn visit<F: FnMut(Key)>(&self, index: u16, visitor: &mut F) {
        if index == NONE {
            return;
        }
        let node = &self.nodes[index as usize];
        self.visit(node.left, visitor);
        visitor(Key::new(node.deadline, node.id));
        self.visit(node.right, visitor);
    }

    fn rebalance_from(&mut self, mut cursor: u16) {
        while cursor != NONE {
            self.update_height(cursor);
            let balance = self.balance(cursor);
            let mut top = cursor;
            if balance > 1 {
                let left = self.nodes[cursor as usize].left;
                if self.balance(left) < 0 {
                    let _ = self.rotate_left(left);
                }
                top = self.rotate_right(cursor);
            } else if balance < -1 {
                let right = self.nodes[cursor as usize].right;
                if self.balance(right) > 0 {
                    let _ = self.rotate_right(right);
                }
                top = self.rotate_left(cursor);
            }
            cursor = self.nodes[top as usize].parent;
        }
    }

    fn rotate_left(&mut self, x: u16) -> u16 {
        let y = self.nodes[x as usize].right;
        debug_assert!(y != NONE);
        let beta = self.nodes[y as usize].left;
        let parent = self.nodes[x as usize].parent;

        self.nodes[x as usize].right = beta;
        if beta != NONE {
            self.nodes[beta as usize].parent = x;
        }
        self.nodes[y as usize].parent = parent;
        if parent == NONE {
            self.root = y;
        } else if self.nodes[parent as usize].left == x {
            self.nodes[parent as usize].left = y;
        } else {
            self.nodes[parent as usize].right = y;
        }
        self.nodes[y as usize].left = x;
        self.nodes[x as usize].parent = y;

        self.update_height(x);
        self.update_height(y);
        y
    }

    fn rotate_right(&mut self, x: u16) -> u16 {
        let y = self.nodes[x as usize].left;
        debug_assert!(y != NONE);
        let beta = self.nodes[y as usize].right;
        let parent = self.nodes[x as usize].parent;

        self.nodes[x as usize].left = beta;
        if beta != NONE {
            self.nodes[beta as usize].parent = x;
        }
        self.nodes[y as usize].parent = parent;
        if parent == NONE {
            self.root = y;
        } else if self.nodes[parent as usize].left == x {
            self.nodes[parent as usize].left = y;
        } else {
            self.nodes[parent as usize].right = y;
        }
        self.nodes[y as usize].right = x;
        self.nodes[x as usize].parent = y;

        self.update_height(x);
        self.update_height(y);
        y
    }

    /// Check parent links, heights, balance and ordering. Returns node count.
    pub fn validate(&self) -> Result<usize, TreeError> {
        if self.root == NONE {
            return if self.len == 0 {
                Ok(0)
            } else {
                Err(TreeError::Invalid)
            };
        }
        if self.nodes[self.root as usize].parent != NONE {
            return Err(TreeError::Invalid);
        }
        let (_, count, _, _) = self.check(self.root, NONE, None, None, 0)?;
        if count != self.len {
            return Err(TreeError::Invalid);
        }
        Ok(count)
    }

    #[allow(clippy::type_complexity)]
    fn check(
        &self,
        index: u16,
        expected_parent: u16,
        lower: Option<Key>,
        upper: Option<Key>,
        depth: usize,
    ) -> Result<(u8, usize, Key, Key), TreeError> {
        if index == NONE || index as usize >= N || depth > N {
            return Err(TreeError::Invalid);
        }
        let node = &self.nodes[index as usize];
        if !node.used || node.parent != expected_parent {
            return Err(TreeError::Invalid);
        }
        let key = Key::new(node.deadline, node.id);
        if let Some(lower) = lower {
            if key <= lower {
                return Err(TreeError::Invalid);
            }
        }
        if let Some(upper) = upper {
            if key >= upper {
                return Err(TreeError::Invalid);
            }
        }

        let (left_height, left_count, left_min, _left_max) = if node.left == NONE {
            (0u8, 0usize, key, key)
        } else {
            self.check(node.left, index, lower, Some(key), depth + 1)?
        };
        let (right_height, right_count, _right_min, right_max) = if node.right == NONE {
            (0u8, 0usize, key, key)
        } else {
            self.check(node.right, index, Some(key), upper, depth + 1)?
        };

        let expected_height = 1 + if left_height > right_height {
            left_height
        } else {
            right_height
        };
        if node.height != expected_height {
            return Err(TreeError::Invalid);
        }
        let difference = left_height as i16 - right_height as i16;
        if difference < -1 || difference > 1 {
            return Err(TreeError::Invalid);
        }

        let min = if left_count == 0 { key } else { left_min };
        let max = if right_count == 0 { key } else { right_max };
        Ok((
            node.height,
            1 + left_count + right_count,
            min,
            max,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect<const N: usize>(tree: &RunTree<N>) -> [u32; N] {
        let mut out = [u32::MAX; N];
        let mut index = 0usize;
        tree.for_each(&mut |key| {
            if index < N {
                out[index] = key.id;
                index += 1;
            }
        });
        out
    }

    #[test]
    fn ascending_insert_balances_and_orders() {
        let mut tree = RunTree::<16>::new();
        for id in 0..16 {
            tree.insert(Key::new(id as u64, id)).unwrap();
            assert_eq!(tree.validate(), Ok(id as usize + 1));
        }
        assert_eq!(tree.len(), 16);
        let ids = collect(&tree);
        assert_eq!(ids, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        assert_eq!(tree.first(), Some(Key::new(0, 0)));
        assert_eq!(tree.last(), Some(Key::new(15, 15)));
        assert!(tree.insert(Key::new(100, 100)).is_err());
    }

    #[test]
    fn descending_insert_balances() {
        let mut tree = RunTree::<16>::new();
        for id in (0..16u32).rev() {
            tree.insert(Key::new(id as u64, id)).unwrap();
            assert_eq!(tree.validate(), Ok((16 - id) as usize));
        }
        let ids = collect(&tree);
        assert_eq!(ids, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
    }

    #[test]
    fn deadlines_are_primary_and_ids_break_ties() {
        let mut tree = RunTree::<8>::new();
        tree.insert(Key::new(5, 9)).unwrap();
        tree.insert(Key::new(5, 3)).unwrap();
        tree.insert(Key::new(4, 7)).unwrap();
        tree.insert(Key::new(6, 1)).unwrap();
        tree.validate().unwrap();
        assert_eq!(tree.first(), Some(Key::new(4, 7)));
        assert_eq!(tree.last(), Some(Key::new(6, 1)));
        assert_eq!(tree.first_at_or_after(5), Some(Key::new(5, 3)));
        assert_eq!(tree.first_at_or_after(6), Some(Key::new(6, 1)));
        assert_eq!(tree.first_at_or_after(7), None);
    }

    #[test]
    fn duplicate_and_missing_keys_are_rejected() {
        let mut tree = RunTree::<4>::new();
        tree.insert(Key::new(1, 1)).unwrap();
        assert_eq!(tree.insert(Key::new(1, 1)), Err(TreeError::Duplicate));
        assert_eq!(tree.remove(Key::new(2, 2)), Err(TreeError::NotFound));
        assert_eq!(tree.remove(Key::new(1, 1)), Ok(()));
        assert_eq!(tree.validate(), Ok(0));
    }

    #[test]
    fn pseudo_random_insert_remove_keeps_invariants() {
        let mut tree = RunTree::<32>::new();
        let mut present = [false; 32];
        let mut state = 0x1234_5678u64;
        for _ in 0..400 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let id = ((state >> 33) as u32) % 32;
            if present[id as usize] {
                tree.remove(Key::new(id as u64, id)).unwrap();
                present[id as usize] = false;
            } else {
                tree.insert(Key::new(id as u64, id)).unwrap();
                present[id as usize] = true;
            }
            tree.validate().unwrap();
            assert_eq!(tree.len(), present.iter().filter(|value| **value).count());
        }
        for _ in 0..32 {
            if tree.first().is_some() {
                tree.remove_first().unwrap();
                tree.validate().unwrap();
            }
        }
        assert!(tree.is_empty());
    }

    #[test]
    fn remove_last_repeatedly() {
        let mut tree = RunTree::<12>::new();
        for id in 0..12 {
            tree.insert(Key::new(100 - id as u64, id)).unwrap();
        }
        for expected in 0..12u32 {
            let key = tree.remove_last().unwrap();
            assert_eq!(key.id, expected);
            tree.validate().unwrap();
        }
        assert_eq!(tree.remove_last(), None);
    }
}
