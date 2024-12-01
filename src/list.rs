use alloc::sync::{Arc, Weak};
use core::marker::PhantomData;
use core::ops::Deref;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;

use crate::RcuCell;

pub struct Node<T> {
    version: Arc<AtomicUsize>,
    next: Arc<RcuCell<Node<T>>>,
    prev: Weak<RcuCell<Node<T>>>, // use Weak to avoid reference cycles
    data: Arc<Option<T>>,         // use Arc to allow data clone
}

impl<T> Node<T> {
    fn clone_from_arc_entry(entry: &Arc<Node<T>>) -> Self {
        Self {
            version: Arc::new(AtomicUsize::new(0)),
            prev: entry.prev.clone(),
            next: entry.next.clone(),
            data: entry.data.clone(),
        }
    }

    fn try_lock(&self) -> bool {
        self.version
            .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    fn lock(&self) {
        while !self.try_lock() {
            core::hint::spin_loop();
        }
    }

    fn unlock(&self) {
        self.version.store(0, Ordering::Relaxed);
    }
}

pub struct Entry<T> {
    node: Arc<RcuCell<Node<T>>>,
}

impl<T> Entry<T> {
    fn as_impl(&self) -> EntryImpl<T> {
        EntryImpl(&self.node)
    }

    /// insert a node after the current node
    pub fn insert_after(&self, data: T) -> Entry<T> {
        Entry {
            node: self.as_impl().insert_after(data),
        }
    }

    /// insert a node before the current node
    pub fn insert_ahead(&self, data: T) -> Entry<T> {
        Entry {
            node: self.as_impl().insert_ahead(data),
        }
    }

    /// remove the node from the list
    pub fn remove(self) {
        let prev_node = self.as_impl().lock_prev_node();
        let curr = self.node.read().unwrap().clone();
        let prev = curr.prev.upgrade().unwrap();
        let next = curr.next.read().unwrap().clone();
        curr.lock();
        prev.write(Node {
            version: prev_node.version.clone(),
            prev: prev_node.prev.clone(),
            next: curr.next.clone(),
            data: prev_node.data.clone(),
        });
        curr.next.write(Node {
            version: next.version.clone(),
            prev: curr.prev.clone(),
            next: next.next.clone(),
            data: next.data.clone(),
        });
        curr.unlock();
        prev_node.unlock();
    }
}

pub struct EntryImpl<'a, T>(&'a Arc<RcuCell<Node<T>>>);

impl<'a, T> EntryImpl<'a, T> {
    /// lock the prev node
    fn lock_prev_node(&self) -> Arc<Node<T>> {
        loop {
            let prev = self.0.read().unwrap();
            prev.lock();
            if !Arc::ptr_eq(&self.0, &prev.next) {
                prev.unlock();
                continue;
            }
            // successfully lock the prev node
            return prev;
        }
    }

    /// insert a node after the current node
    fn insert_after(&self, data: T) -> Arc<RcuCell<Node<T>>> {
        // first lock the current node
        let prev = self.lock_prev_node();
        let new_entry = Arc::new(RcuCell::new(Node {
            version: Arc::new(AtomicUsize::new(0)),
            prev: Arc::downgrade(&self.0),
            next: prev.next.clone(),
            data: Arc::new(Some(data)),
        }));
        // let new_entry = Arc::new(RcuCell::new(Entry::new(elt)));
        // let new_entry_ref = new_entry.read().unwrap();
        // new_entry_ref.prev = Arc::downgrade(&self.head);
        // new_entry_ref.next = next.clone();
        // next.write(new_entry_ref);
        // head.next = new_entry;
        todo!();
    }

    /// insert a node before the current node
    fn insert_ahead(&self, data: T) -> Arc<RcuCell<Node<T>>> {
        // first lock the current node
        let prev = self.lock_prev_node();
        let new_entry = Arc::new(RcuCell::new(Node {
            version: Arc::new(AtomicUsize::new(0)),
            prev: Arc::downgrade(&self.0),
            next: prev.next.clone(),
            data: Arc::new(Some(data)),
        }));

        todo!()
    }


    /// remove the node before the current node
    /// this could invalidate the prev Entry!!
    fn remove_ahead(&self, head: &Arc<RcuCell<Node<T>>>) -> Option<Arc<RcuCell<Node<T>>>> {
        // self.0 is the next node
        loop {
            let curr = self.lock_prev_node();
            let prev = match curr.prev.upgrade() {
                None => {
                    // this is the head
                    curr.unlock();
                    return None;
                }
                // the prev can change due to prev insert/remove
                Some(prev) => prev,
            };
            // there could be dead lock, we must first lock prev
            curr.unlock();
            let prev = prev.read().unwrap();
            prev.lock();
        }
    }

    /// remove the node after the current node
    fn remove_after(&self, tail: &Arc<RcuCell<Node<T>>>) -> Option<Arc<RcuCell<Node<T>>>> {
        // self.0 is the prev node
        let prev = self.0.read().unwrap();
        prev.lock();
        if Arc::ptr_eq(&prev.next, tail) {
            prev.unlock();
            return None;
        }
        let curr = prev.next.read().unwrap();
        curr.lock();
        let ret = prev.next.clone();
        self.0.write(Node {
            version: prev.version.clone(),
            prev: prev.prev.clone(),
            next: curr.next.clone(),
            data: prev.data.clone(),
        });
        let next = curr.next.read().unwrap();
        curr.next.write(Node {
            version: next.version.clone(),
            prev: curr.prev.clone(),
            next: next.next.clone(),
            data: next.data.clone(),
        });
        curr.unlock();
        prev.unlock();
        Some(ret)
    }
}

pub struct EntryRef<T>(Arc<Node<T>>);

impl<T> Deref for EntryRef<T> {
    type Target = T;
    fn deref(&self) -> &T {
        (*self.0.data).as_ref().unwrap()
    }
}

impl<T> EntryRef<T> {
    // /// get the next node
    // pub fn next(&self) -> Option<EntryRef<T>> {
    // 	let next = self.0.next.read().unwrap();
    // 	(!Arc::ptr_eq(&next, &self.0.next)).then(|| EntryRef(next))
    // }

    // /// get the previous node
    // pub fn prev(&self) -> Option<EntryRef<T>> {
    // 	let prev = self.0.prev.upgrade().unwrap();
    // 	(!Arc::ptr_eq(&prev, &self.0.prev)).then(|| EntryRef(prev))
    // }
}

pub struct LinkedList<T> {
    head: Arc<RcuCell<Node<T>>>,
    tail: Arc<RcuCell<Node<T>>>,
}

impl<T> Drop for LinkedList<T> {
    fn drop(&mut self) {
        while let Some(_) = self.pop_front() {}
    }
}

impl<T> LinkedList<T> {
    pub fn new() -> Self {
        let tail = Arc::new(RcuCell::new(Node {
            version: Arc::new(AtomicUsize::new(0)),
            prev: Weak::new(),
            next: Arc::new(RcuCell::none()),
            // this is only used for list head, should never access
            data: Arc::new(None),
        }));

        let head = Arc::new(RcuCell::new(Node {
            version: Arc::new(AtomicUsize::new(0)),
            prev: Weak::new(),
            next: tail.clone(),
            // this is only used for list head, should never access
            data: Arc::new(None),
        }));

        let mut tail_entry = Node::clone_from_arc_entry(&tail.read().unwrap());
        tail_entry.prev = Arc::downgrade(&head);

        tail.write(tail_entry);

        Self { head, tail }
    }

    pub fn front(&self) -> Option<EntryRef<T>> {
        let front = self.head.read().unwrap();
        (!Arc::ptr_eq(&front.next, &self.tail)).then(|| {
            let node = front.next.read().unwrap();
            EntryRef(node)
        })
    }

    pub fn back(&self) -> Option<EntryRef<T>> {
        let tail = self.tail.read().unwrap();
        let prev = tail.prev.upgrade().unwrap();
        (!Arc::ptr_eq(&prev, &self.head)).then(|| {
            let node = prev.read().unwrap();
            EntryRef(node)
        })
    }

    pub fn push_back(&mut self, elt: T) -> EntryRef<T> {
        let entry = EntryImpl(&self.tail);
        let new_entry = entry.insert_ahead(elt);
        new_entry.read().map(EntryRef).unwrap()
    }

    pub fn pop_back(&mut self) -> Option<EntryRef<T>> {
        let entry = EntryImpl(&self.tail);
        entry
            .remove_ahead(&self.head)
            .and_then(|node| node.read().map(EntryRef))
    }

    pub fn push_front(&mut self, elt: T) -> EntryRef<T> {
        let entry = EntryImpl(&self.head);
        let new_entry = entry.insert_after(elt);
        new_entry.read().map(EntryRef).unwrap()
    }

    pub fn pop_front(&mut self) -> Option<EntryRef<T>> {
        let entry = EntryImpl(&self.head);
        entry
            .remove_after(&self.tail)
            .and_then(|node| node.read().map(EntryRef))
    }

    pub fn iter(&self) -> Iter<T> {
        Iter {
            head: self.head.clone(),
            tail: self.tail.clone(),
            curr: self.head.clone(),
            marker: PhantomData,
        }
    }
}

/// An iterator over the elements of a `LinkedList`.
///
/// This `struct` is created by [`LinkedList::iter()`]. See its
/// documentation for more.
pub struct Iter<'a, T: 'a> {
    head: Arc<RcuCell<Node<T>>>,
    tail: Arc<RcuCell<Node<T>>>,
    curr: Arc<RcuCell<Node<T>>>,
    marker: PhantomData<&'a Node<T>>,
}

impl<T> Iterator for Iter<'_, T> {
    type Item = EntryRef<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if Arc::ptr_eq(&self.curr, &self.tail) {
            return None;
        }

        let curr = self.curr.read().unwrap();
        self.curr = curr.next.clone();
        Some(EntryRef(curr))
    }
}

impl<T> DoubleEndedIterator for Iter<'_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if Arc::ptr_eq(&self.curr, &self.head) {
            return None;
        }

        let curr = self.curr.read().unwrap();
        // TODO: need to check prev deleted
        self.curr = curr.prev.upgrade().unwrap();
        Some(EntryRef(curr))
    }
}
