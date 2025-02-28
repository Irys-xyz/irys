use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

#[derive(Debug)]
pub struct ClonableJoinHandle<T> {
    inner: Arc<Mutex<Option<JoinHandle<T>>>>,
}

impl<T> Clone for ClonableJoinHandle<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> ClonableJoinHandle<T> {
    pub fn new(handle: JoinHandle<T>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(handle))),
        }
    }

    pub fn join(&self) -> thread::Result<T> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(handle) = guard.take() {
            handle.join()
        } else {
            panic!("JoinHandle already consumed");
        }
    }
}

impl<T> From<JoinHandle<T>> for ClonableJoinHandle<T> {
    fn from(handle: JoinHandle<T>) -> Self {
        Self::new(handle)
    }
}

#[derive(Debug)]
pub struct DestroyableArc<T> {
    inner: Arc<Mutex<Option<T>>>,
}

impl<T> Clone for DestroyableArc<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> DestroyableArc<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(value))),
        }
    }

    pub fn destroy(&self) -> T {
        let mut guard = self.inner.lock().unwrap();
        if let Some(value) = guard.take() {
            value
        } else {
            panic!("Value already consumed");
        }
    }
}
