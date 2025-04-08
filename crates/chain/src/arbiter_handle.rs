use actix_rt::Arbiter;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

#[derive(Debug)]
pub struct ArbiterHandle {
    inner: Arc<Mutex<Option<Arbiter>>>,
    pub name: String,
}

impl Clone for ArbiterHandle {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            inner: Arc::clone(&self.inner),
        }
    }
}

impl ArbiterHandle {
    pub fn new(value: Arbiter, name: String) -> Self {
        Self {
            name,
            inner: Arc::new(Mutex::new(Some(value))),
        }
    }

    pub fn take(&self) -> Arbiter {
        let mut guard = self.inner.lock().unwrap();
        if let Some(value) = guard.take() {
            value
        } else {
            panic!("Value already consumed");
        }
    }

    pub fn stop_and_join(self) {
        let arbiter = self.take();
        arbiter.stop();
        arbiter.join().unwrap();
    }
}

#[derive(Debug)]
pub struct CloneableJoinHandle<T> {
    inner: Arc<Mutex<Option<JoinHandle<T>>>>,
}

impl<T> Clone for CloneableJoinHandle<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> CloneableJoinHandle<T> {
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

impl<T> From<JoinHandle<T>> for CloneableJoinHandle<T> {
    fn from(handle: JoinHandle<T>) -> Self {
        Self::new(handle)
    }
}
