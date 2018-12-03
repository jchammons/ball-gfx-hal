/// Generic double buffer.
#[derive(Copy, Clone, Debug, Default)]
pub struct DoubleBuffer<T> {
    current: usize,
    items: [T; 2],
}

impl<T> DoubleBuffer<T> {
    /// Sets both items to the same initial value.
    pub fn new(initial: T) -> DoubleBuffer<T>
    where
        T: Clone,
    {
        DoubleBuffer {
            current: 0,
            items: [initial.clone(), initial.clone()],
        }
    }

    fn old(&self) -> usize {
        (self.current + 1) % 2
    }

    /// Swaps current and next items.
    pub fn swap(&mut self) {
        self.current = self.old();
    }

    /// Inserts a value, replacing the older item.
    ///
    /// This does not automatically swap the buffers.
    pub fn insert(&mut self, item: T) {
        self.items[self.old()] = item;
    }

    /// Gets the latest item.
    pub fn get(&self) -> &T {
        &self.items[self.current]
    }

    /// Gets the older item.
    pub fn get_old(&self) -> &T {
        &self.items[self.old()]
    }
}
