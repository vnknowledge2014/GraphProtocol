//! Persistent structurally shared ordered map (`OrdMap`) backed by an `Arc`-shared AVL tree
//! (`spec/01-architecture.md` §4, `ARC-005`, `ADR-002`).
//!
//! Replacing `im::OrdMap` removes all unmaintained/unsound third-party advisories (`RUSTSEC-2023-0126`,
//! `RUSTSEC-2026-0247`, `RUSTSEC-2026-0248`, `RUSTSEC-2026-0251`) while keeping $O(\log N)$ copy-on-write
//! structural sharing across [`Snapshot`](crate::snapshot::Snapshot) generations with zero `unsafe` code.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt;
use std::iter::FusedIterator;
use std::sync::Arc;

#[derive(Clone)]
struct Node<K, V> {
    key: K,
    value: V,
    height: u8,
    left: Option<Arc<Node<K, V>>>,
    right: Option<Arc<Node<K, V>>>,
}

impl<K, V> Node<K, V> {
    fn new(key: K, value: V) -> Self {
        Self {
            key,
            value,
            height: 1,
            left: None,
            right: None,
        }
    }

    fn update_height(&mut self) {
        let lh = height_of(&self.left);
        let rh = height_of(&self.right);
        self.height = lh.max(rh).saturating_add(1);
    }

    fn balance_factor(&self) -> i16 {
        i16::from(height_of(&self.left)) - i16::from(height_of(&self.right))
    }
}

fn height_of<K, V>(node: &Option<Arc<Node<K, V>>>) -> u8 {
    node.as_ref().map_or(0, |n| n.height)
}

fn rotate_right<K: Clone, V: Clone>(root: &mut Arc<Node<K, V>>) {
    let root_mut = Arc::make_mut(root);
    let Some(mut pivot_arc) = root_mut.left.take() else {
        return;
    };
    let pivot_mut = Arc::make_mut(&mut pivot_arc);
    root_mut.left = pivot_mut.right.take();
    root_mut.update_height();
    let old_root = std::mem::replace(root, pivot_arc);
    let new_root = Arc::make_mut(root);
    new_root.right = Some(old_root);
    new_root.update_height();
}

fn rotate_left<K: Clone, V: Clone>(root: &mut Arc<Node<K, V>>) {
    let root_mut = Arc::make_mut(root);
    let Some(mut pivot_arc) = root_mut.right.take() else {
        return;
    };
    let pivot_mut = Arc::make_mut(&mut pivot_arc);
    root_mut.right = pivot_mut.left.take();
    root_mut.update_height();
    let old_root = std::mem::replace(root, pivot_arc);
    let new_root = Arc::make_mut(root);
    new_root.left = Some(old_root);
    new_root.update_height();
}

fn rebalance<K: Clone, V: Clone>(slot: &mut Option<Arc<Node<K, V>>>) {
    let Some(root) = slot else {
        return;
    };
    Arc::make_mut(root).update_height();
    let bf = root.balance_factor();
    if bf > 1 {
        let left_heavy_right = root.left.as_ref().is_some_and(|l| l.balance_factor() < 0);
        if left_heavy_right && let Some(left) = Arc::make_mut(root).left.as_mut() {
            rotate_left(left);
        }
        rotate_right(root);
    } else if bf < -1 {
        let right_heavy_left = root.right.as_ref().is_some_and(|r| r.balance_factor() > 0);
        if right_heavy_left && let Some(right) = Arc::make_mut(root).right.as_mut() {
            rotate_right(right);
        }
        rotate_left(root);
    }
}

fn insert_rec<K: Ord + Clone, V: Clone>(
    slot: &mut Option<Arc<Node<K, V>>>,
    key: K,
    value: V,
) -> Option<V> {
    let Some(arc) = slot else {
        *slot = Some(Arc::new(Node::new(key, value)));
        return None;
    };
    let node = Arc::make_mut(arc);
    let old = match key.cmp(&node.key) {
        Ordering::Equal => {
            node.key = key;
            return Some(std::mem::replace(&mut node.value, value));
        }
        Ordering::Less => insert_rec(&mut node.left, key, value),
        Ordering::Greater => insert_rec(&mut node.right, key, value),
    };
    if old.is_none() {
        rebalance(slot);
    }
    old
}

fn take_min_rec<K: Clone, V: Clone>(slot: &mut Option<Arc<Node<K, V>>>) -> Option<(K, V)> {
    let arc = slot.as_mut()?;
    if arc.left.is_none() {
        let taken = slot.take()?;
        let kv = match Arc::try_unwrap(taken) {
            Ok(owned) => {
                *slot = owned.right;
                (owned.key, owned.value)
            }
            Err(shared) => {
                *slot = shared.right.clone();
                (shared.key.clone(), shared.value.clone())
            }
        };
        return Some(kv);
    }
    let node = Arc::make_mut(arc);
    let min_kv = take_min_rec(&mut node.left);
    rebalance(slot);
    min_kv
}

fn remove_rec<K, V, Q>(slot: &mut Option<Arc<Node<K, V>>>, key: &Q) -> Option<V>
where
    K: Borrow<Q> + Ord + Clone,
    V: Clone,
    Q: Ord + ?Sized,
{
    let cmp = {
        let arc = slot.as_ref()?;
        key.cmp(arc.key.borrow())
    };
    match cmp {
        Ordering::Less => {
            let arc = slot.as_mut()?;
            let node = Arc::make_mut(arc);
            let removed = remove_rec(&mut node.left, key);
            if removed.is_some() {
                rebalance(slot);
            }
            removed
        }
        Ordering::Greater => {
            let arc = slot.as_mut()?;
            let node = Arc::make_mut(arc);
            let removed = remove_rec(&mut node.right, key);
            if removed.is_some() {
                rebalance(slot);
            }
            removed
        }
        Ordering::Equal => {
            let one_or_zero_children = slot
                .as_ref()
                .is_some_and(|a| a.left.is_none() || a.right.is_none());
            if one_or_zero_children {
                let taken = slot.take()?;
                let val = match Arc::try_unwrap(taken) {
                    Ok(owned) => {
                        *slot = owned.left.or(owned.right);
                        owned.value
                    }
                    Err(shared) => {
                        *slot = shared.left.clone().or_else(|| shared.right.clone());
                        shared.value.clone()
                    }
                };
                return Some(val);
            }
            let arc = slot.as_mut()?;
            let node = Arc::make_mut(arc);
            let (succ_k, succ_v) = take_min_rec(&mut node.right)?;
            node.key = succ_k;
            let old_val = std::mem::replace(&mut node.value, succ_v);
            rebalance(slot);
            Some(old_val)
        }
    }
}

/// Persistent ordered map using copy-on-write `Arc` AVL tree nodes (`ARC-005`).
///
/// Cloning an [`OrdMap`] is an $O(1)$ reference-count increment. Mutations on a shared map copy only
/// the $O(\log N)$ modified path while keeping all untouched subtrees pointer-shared across snapshot
/// generations.
pub struct OrdMap<K, V> {
    root: Option<Arc<Node<K, V>>>,
    len: usize,
}

impl<K, V> Clone for OrdMap<K, V> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            len: self.len,
        }
    }
}

impl<K, V> Default for OrdMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: fmt::Debug + Ord, V: fmt::Debug> fmt::Debug for OrdMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: PartialEq, V: PartialEq> PartialEq for OrdMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<K: Eq, V: Eq> Eq for OrdMap<K, V> {}

impl<K, V> OrdMap<K, V> {
    /// Creates an empty [`OrdMap`].
    #[must_use]
    pub const fn new() -> Self {
        Self { root: None, len: 0 }
    }

    /// Returns the number of entries in the map.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when the map contains zero entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Clears all entries from the map.
    pub fn clear(&mut self) {
        self.root = None;
        self.len = 0;
    }

    /// Returns a reference to the value associated with `key`, if present.
    #[must_use]
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        let mut cur = self.root.as_deref();
        while let Some(node) = cur {
            match key.cmp(node.key.borrow()) {
                Ordering::Equal => return Some(&node.value),
                Ordering::Less => cur = node.left.as_deref(),
                Ordering::Greater => cur = node.right.as_deref(),
            }
        }
        None
    }

    /// Returns `true` if `key` is present in the map.
    #[must_use]
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        self.get(key).is_some()
    }

    /// Inserts `(key, value)` into the map in $O(\log N)$ copy-on-write steps, returning the
    /// previous value if `key` was already present.
    pub fn insert(&mut self, key: K, value: V) -> Option<V>
    where
        K: Ord + Clone,
        V: Clone,
    {
        let prev = insert_rec(&mut self.root, key, value);
        if prev.is_none() {
            self.len += 1;
        }
        prev
    }

    /// Removes `key` from the map if present, leaving the tree completely untouched when absent.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q> + Ord + Clone,
        V: Clone,
        Q: Ord + ?Sized,
    {
        if !self.contains_key(key) {
            return None;
        }
        let removed = remove_rec(&mut self.root, key);
        if removed.is_some() {
            self.len = self.len.saturating_sub(1);
        }
        removed
    }

    /// Iterates over all `(key, value)` pairs in ascending key order.
    #[must_use]
    pub fn iter(&self) -> Iter<'_, K, V> {
        let mut iter = Iter {
            stack: Vec::new(),
            remaining: self.len,
        };
        iter.push_left(self.root.as_deref());
        iter
    }

    /// Iterates over all keys in ascending order.
    #[must_use]
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys { inner: self.iter() }
    }

    /// Iterates over all values ordered by ascending key.
    #[must_use]
    pub fn values(&self) -> Values<'_, K, V> {
        Values { inner: self.iter() }
    }
}

/// Ascending-order iterator over entries of an [`OrdMap`].
pub struct Iter<'a, K, V> {
    stack: Vec<&'a Node<K, V>>,
    remaining: usize,
}

impl<'a, K, V> Iter<'a, K, V> {
    fn push_left(&mut self, mut node: Option<&'a Node<K, V>>) {
        while let Some(n) = node {
            self.stack.push(n);
            node = n.left.as_deref();
        }
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        self.remaining = self.remaining.saturating_sub(1);
        self.push_left(node.right.as_deref());
        Some((&node.key, &node.value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {}
impl<K, V> FusedIterator for Iter<'_, K, V> {}

/// Ascending-key iterator for [`OrdMap::keys`].
pub struct Keys<'a, K, V> {
    inner: Iter<'a, K, V>,
}

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, _)| k)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for Keys<'_, K, V> {}
impl<K, V> FusedIterator for Keys<'_, K, V> {}

/// Key-ordered value iterator for [`OrdMap::values`].
pub struct Values<'a, K, V> {
    inner: Iter<'a, K, V>,
}

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(_, v)| v)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for Values<'_, K, V> {}
impl<K, V> FusedIterator for Values<'_, K, V> {}

impl<'a, K, V> IntoIterator for &'a OrdMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K: Ord + Clone, V: Clone> FromIterator<(K, V)> for OrdMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut map = Self::new();
        for (k, v) in iter {
            map.insert(k, v);
        }
        map
    }
}

impl<K: Ord + Clone, V: Clone> Extend<(K, V)> for OrdMap<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<K: Ord + Clone, V: Clone> From<std::collections::BTreeMap<K, V>> for OrdMap<K, V> {
    fn from(map: std::collections::BTreeMap<K, V>) -> Self {
        map.into_iter().collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;

    fn assert_avl_invariants<K: Ord, V>(node: &Option<Arc<Node<K, V>>>) -> u8 {
        let Some(n) = node else {
            return 0;
        };
        let lh = assert_avl_invariants(&n.left);
        let rh = assert_avl_invariants(&n.right);
        assert_eq!(n.height, lh.max(rh) + 1, "height mismatch");
        let bf = i16::from(lh) - i16::from(rh);
        assert!(
            (-1..=1).contains(&bf),
            "AVL balance factor out of bounds: {bf}"
        );
        if let Some(l) = &n.left {
            assert!(l.key < n.key, "BST left invariant violated");
        }
        if let Some(r) = &n.right {
            assert!(r.key > n.key, "BST right invariant violated");
        }
        n.height
    }

    // spec: ARC-005
    #[test]
    fn structural_sharing_keeps_untouched_subtree_arcs_at_single_strong_count() {
        let mut g1 = OrdMap::new();
        let vals: Vec<Arc<u32>> = (0..256_u32).map(Arc::new).collect();
        for (idx, v) in vals.iter().enumerate() {
            g1.insert(idx, Arc::clone(v));
        }
        drop(vals);

        let mut g2 = g1.clone();
        g2.insert(255, Arc::new(9999));

        assert_avl_invariants(&g1.root);
        assert_avl_invariants(&g2.root);

        let untouched = (0..255_usize)
            .filter(|k| Arc::strong_count(g2.get(k).expect("present")) == 1)
            .count();
        assert!(
            untouched >= 230,
            "expected >= 230/255 shared subtree values (strong_count == 1), got {untouched}"
        );
        assert_eq!(*g1.get(&255).expect("g1 255"), Arc::new(255));
        assert_eq!(*g2.get(&255).expect("g2 255"), Arc::new(9999));
    }

    // spec: ARC-005
    proptest! {
        #[test]
        fn ordmap_matches_btreemap_across_random_insert_remove_sequences(
            ops in proptest::collection::vec((any::<bool>(), 0_u16..96, any::<u32>()), 1..200)
        ) {
            let mut persistent = OrdMap::new();
            let mut model = BTreeMap::new();
            let mut checkpoints = Vec::new();

            for (step, (is_insert, k, v)) in ops.into_iter().enumerate() {
                if is_insert {
                    prop_assert_eq!(persistent.insert(k, v), model.insert(k, v));
                } else {
                    prop_assert_eq!(persistent.remove(&k), model.remove(&k));
                }
                assert_avl_invariants(&persistent.root);
                prop_assert_eq!(persistent.len(), model.len());
                if step % 17 == 0 {
                    checkpoints.push((persistent.clone(), model.clone()));
                }
            }

            let actual: Vec<(u16, u32)> = persistent.iter().map(|(&k, &v)| (k, v)).collect();
            let expected: Vec<(u16, u32)> = model.iter().map(|(&k, &v)| (k, v)).collect();
            prop_assert_eq!(actual, expected);

            for (snap_p, snap_m) in checkpoints {
                let act: Vec<(u16, u32)> = snap_p.iter().map(|(&k, &v)| (k, v)).collect();
                let exp: Vec<(u16, u32)> = snap_m.iter().map(|(&k, &v)| (k, v)).collect();
                prop_assert_eq!(act, exp);
            }
        }
    }
}
