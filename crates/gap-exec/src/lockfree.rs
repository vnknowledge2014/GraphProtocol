//! Lock-free append-only `OnceLock` storage primitives (`INV-02`).

use std::sync::OnceLock;

const CHUNK_CAP: usize = 64;

#[derive(Debug)]
struct LogChunk<T> {
    slots: [OnceLock<T>; CHUNK_CAP],
    next: OnceLock<Box<LogChunk<T>>>,
}

impl<T> Default for LogChunk<T> {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| OnceLock::new()),
            next: OnceLock::new(),
        }
    }
}

/// Lock-free, append-only chunked log backed exclusively by [`OnceLock`] (`INV-02`).
#[derive(Debug)]
pub(crate) struct LockFreeLog<T> {
    head: LogChunk<T>,
}

impl<T> Default for LockFreeLog<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> LockFreeLog<T> {
    /// Creates an empty [`LockFreeLog`].
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            head: LogChunk::default(),
        }
    }

    /// Appends `value` to the tail of the log without locking.
    pub(crate) fn push(&self, mut value: T) {
        let mut chunk = &self.head;
        loop {
            for slot in &chunk.slots {
                if slot.get().is_none() {
                    match slot.set(value) {
                        Ok(()) => return,
                        Err(returned) => value = returned,
                    }
                }
            }
            chunk = chunk.next.get_or_init(|| Box::new(LogChunk::default()));
        }
    }

    /// Inserts `value` only if no equal entry already exists; returns `false` on duplicate.
    pub(crate) fn insert_unique(&self, mut value: T) -> bool
    where
        T: PartialEq,
    {
        let mut chunk = &self.head;
        loop {
            for slot in &chunk.slots {
                if let Some(existing) = slot.get() {
                    if *existing == value {
                        return false;
                    }
                } else {
                    match slot.set(value) {
                        Ok(()) => return true,
                        Err(returned) => {
                            value = returned;
                            if let Some(existing) = slot.get()
                                && *existing == value
                            {
                                return false;
                            }
                        }
                    }
                }
            }
            chunk = chunk.next.get_or_init(|| Box::new(LogChunk::default()));
        }
    }

    /// Returns a reference to the first item satisfying `pred`, or atomically appends `make()`.
    pub(crate) fn get_or_insert_with<F>(&self, pred: impl Fn(&T) -> bool, make: F) -> &T
    where
        F: Fn() -> T,
    {
        let mut candidate: Option<T> = None;
        let mut chunk = &self.head;
        loop {
            for slot in &chunk.slots {
                if let Some(existing) = slot.get() {
                    if pred(existing) {
                        return existing;
                    }
                } else {
                    let val = match candidate.take() {
                        Some(v) => v,
                        None => make(),
                    };
                    match slot.set(val) {
                        Ok(()) => {
                            if let Some(inserted) = slot.get() {
                                return inserted;
                            }
                        }
                        Err(returned) => {
                            candidate = Some(returned);
                            if let Some(existing) = slot.get()
                                && pred(existing)
                            {
                                return existing;
                            }
                        }
                    }
                }
            }
            chunk = chunk.next.get_or_init(|| Box::new(LogChunk::default()));
        }
    }

    /// Returns an iterator over all initialized entries in insertion order.
    pub(crate) fn iter(&self) -> LockFreeIter<'_, T> {
        LockFreeIter {
            chunk: Some(&self.head),
            index: 0,
        }
    }

    /// Returns the entry at zero-based position `index`, if present.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn nth(&self, index: usize) -> Option<&T> {
        self.iter().nth(index)
    }

    /// Returns the number of entries currently recorded in the log.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.iter().count()
    }

    /// Returns `true` if no entries have been recorded yet.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    /// Clones all currently recorded entries into a `Vec<T>`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn to_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.iter().cloned().collect()
    }
}

/// Iterator over initialized entries in a [`LockFreeLog`].
pub(crate) struct LockFreeIter<'a, T> {
    chunk: Option<&'a LogChunk<T>>,
    index: usize,
}

impl<'a, T> Iterator for LockFreeIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let chunk = self.chunk?;
            while self.index < CHUNK_CAP {
                let idx = self.index;
                self.index += 1;
                if let Some(val) = chunk.slots[idx].get() {
                    return Some(val);
                }
            }
            self.chunk = chunk.next.get().map(Box::as_ref);
            self.index = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LockFreeLog;

    // spec: INV-02
    #[test]
    fn lock_free_log_handles_multi_chunk_push_unique_and_get_or_insert() {
        let log = LockFreeLog::new();
        assert!(log.is_empty());
        for i in 0..140_usize {
            assert!(log.insert_unique(i));
            assert!(!log.insert_unique(i));
        }
        assert_eq!(log.len(), 140);
        assert_eq!(log.nth(0), Some(&0));
        assert_eq!(log.nth(139), Some(&139));
        let found = log.get_or_insert_with(|v| *v == 75, || 999);
        assert_eq!(*found, 75);
        assert_eq!(log.len(), 140);
        let inserted = log.get_or_insert_with(|v| *v == 999, || 999);
        assert_eq!(*inserted, 999);
        assert_eq!(log.len(), 141);
    }
}
