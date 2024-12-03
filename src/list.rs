use alloc::sync::{Arc, Weak};
use core::marker::PhantomData;
use core::ops::Deref;
use core::sync::atomic::{AtomicUsize, Ordering};

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

    fn try_lock(&self) -> Result<usize, ()> {
        let version = self.version.load(Ordering::Relaxed);
        if version & 1 == 1 {
            return Err(());
        }
        match self.version.compare_exchange_weak(
            version,
            version + 2,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(version),
            Err(_) => Ok(usize::MAX),
        }
    }

    fn lock(&self) -> Result<usize, ()> {
        let mut version;
        loop {
            version = self.try_lock()?;
            if version != usize::MAX {
                break;
            }
            core::hint::spin_loop();
        }
        Ok(version)
    }

    fn unlock(&self) {
        self.version.fetch_add(2, Ordering::Relaxed);
    }

    fn unlock_remove(&self) {
        self.version.fetch_add(3, Ordering::Relaxed);
    }

    fn is_removed(&self) -> bool {
        self.version.load(Ordering::Relaxed) & 1 == 1
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
    pub fn insert_after(&self, data: T) -> Result<Entry<T>, T> {
        Ok(Entry {
            node: self.as_impl().insert_after(data)?,
        })
    }

    /// insert a node before the current node
    pub fn insert_ahead(&self, data: T) -> Result<Entry<T>, T> {
        Ok(Entry {
            node: self.as_impl().insert_ahead(data)?,
        })
    }

    /// remove the node from the list
    pub fn remove(self) {
        let (prev, prev_node) = match self.as_impl().lock_prev_node() {
            Ok((prev, prev_node)) => (prev, prev_node),
            Err(_) => return,
        };
        let curr_node = self.node.read().unwrap();
        let next_node = curr_node.next.read().unwrap();
        if curr_node.lock().is_err() {
            // current node is removed
            prev_node.unlock();
            return;
        }
        assert!(Arc::ptr_eq(&prev, &curr_node.prev.upgrade().unwrap()));
        // update prev.next
        prev.write(Node {
            version: prev_node.version.clone(),
            prev: prev_node.prev.clone(),
            next: curr_node.next.clone(),
            data: prev_node.data.clone(),
        });
        // update next.prev
        curr_node.next.write(Node {
            version: next_node.version.clone(),
            prev: curr_node.prev.clone(),
            next: next_node.next.clone(),
            data: next_node.data.clone(),
        });
        // make current node removed
        curr_node.unlock_remove();
        prev_node.unlock();
    }
}

pub struct EntryImpl<'a, T>(&'a Arc<RcuCell<Node<T>>>);

impl<'a, T> EntryImpl<'a, T> {
    /// lock the prev node
    fn lock_prev_node(&self) -> Result<(Arc<RcuCell<Node<T>>>, Arc<Node<T>>), ()> {
        loop {
            let curr = self.0.read().unwrap();
            let prev = match curr.prev.upgrade() {
                // something wrong, like the prev node is deleted, or the current node is deleted
                None => {
                    if curr.is_removed() {
                        return Err(());
                    } else {
                        // try again
                        continue;
                    }
                }

                // the prev can change due to prev insert/remove
                Some(prev) => prev,
            };
            let prev_node = prev.read().unwrap();
            if prev_node.lock().is_err() {
                // the prev node is removed
                continue;
            }
            if !Arc::ptr_eq(self.0, &prev_node.next) {
                // the prev node is changed
                prev_node.unlock();
                continue;
            }
            // successfully lock the prev node
            return Ok((prev, prev_node));
        }
    }

    /// insert a node after the current node
    /// this could failed due to the current node is removed
    fn insert_after(&self, data: T) -> Result<Arc<RcuCell<Node<T>>>, T> {
        // first lock the current node
        let curr_node = self.0.read().unwrap();
        if curr_node.lock().is_err() {
            // current node is removed
            return Err(data);
        }
        self.insert_after_locked(curr_node, data)
    }

    fn insert_after_locked(
        &self,
        curr_node: Arc<Node<T>>,
        data: T,
    ) -> Result<Arc<RcuCell<Node<T>>>, T> {
        let next_node = curr_node.next.read().unwrap();
        let new_entry = Arc::new(RcuCell::new(Node {
            version: Arc::new(AtomicUsize::new(0)),
            prev: Arc::downgrade(&self.0),
            next: curr_node.next.clone(),
            data: Arc::new(Some(data)),
        }));

        curr_node.next.write(Node {
            version: next_node.version.clone(),
            prev: Arc::downgrade(&new_entry),
            next: next_node.next.clone(),
            data: next_node.data.clone(),
        });

        self.0.write(Node {
            version: curr_node.version.clone(),
            prev: curr_node.prev.clone(),
            next: new_entry.clone(),
            data: curr_node.data.clone(),
        });
        curr_node.unlock();
        Ok(new_entry)
    }

    /// insert a node before the current node
    /// this could failed due to the current node is removed
    fn insert_ahead(&self, data: T) -> Result<Arc<RcuCell<Node<T>>>, T> {
        // first lock the current node
        let (prev, prev_node) = match self.lock_prev_node() {
            Ok((prev, prev_node)) => (prev, prev_node),
            Err(_) => return Err(data),
        };
        let prev = EntryImpl(&prev);
        prev.insert_after_locked(prev_node, data)
    }

    /// remove the node before the current node
    /// this could invalidate the prev Entry!!
    fn remove_ahead(&self, head: &Arc<RcuCell<Node<T>>>) -> Option<Arc<RcuCell<Node<T>>>> {
        let (prev, prev_node) = match self.lock_prev_node() {
            Ok((prev, prev_node)) => (prev, prev_node),
            Err(_) => return None,
        };
        if Arc::ptr_eq(&prev, head) {
            // we can't remove the head node
            prev_node.unlock();
            return None;
        }
        self.remove_after_locked(prev_node, None)
    }

    /// remove the node after the current node
    /// if the current node is removed, return None
    /// if the next node is tail, return None
    fn remove_after(&self, tail: &Arc<RcuCell<Node<T>>>) -> Option<Arc<RcuCell<Node<T>>>> {
        // self.0 is the prev node
        let curr_node = self.0.read().unwrap();
        curr_node.lock().ok()?;
        self.remove_after_locked(curr_node, Some(tail))
    }

    fn remove_after_locked(
        &self,
        curr_node: Arc<Node<T>>,
        tail: Option<&Arc<RcuCell<Node<T>>>>,
    ) -> Option<Arc<RcuCell<Node<T>>>> {
        // self.0 is the prev node
        let prev_node = curr_node;
        if let Some(tail) = tail {
            if Arc::ptr_eq(&prev_node.next, tail) {
                // we can't remove the tail node
                prev_node.unlock();
                return None;
            }
        }

        let curr_node = prev_node.next.read().unwrap();
        curr_node.lock().unwrap();
        let ret = prev_node.next.clone();
        // update prev.next
        self.0.write(Node {
            version: prev_node.version.clone(),
            prev: prev_node.prev.clone(),
            next: curr_node.next.clone(),
            data: prev_node.data.clone(),
        });

        // update next.prev
        let next_node = curr_node.next.read().unwrap();
        curr_node.next.write(Node {
            version: next_node.version.clone(),
            prev: curr_node.prev.clone(),
            next: next_node.next.clone(),
            data: next_node.data.clone(),
        });
        curr_node.unlock_remove();
        prev_node.unlock();
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
        let new_entry = match entry.insert_ahead(elt) {
            Ok(entry) => entry,
            Err(_) => panic!("push_back failed"),
        };
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
        let new_entry = match entry.insert_after(elt) {
            Ok(entry) => entry,
            Err(_) => panic!("push_front failed"),
        };
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
