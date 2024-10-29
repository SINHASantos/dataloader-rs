use crate::runtime::{Arc, Mutex};
use crate::{yield_fn, BatchFn, WaitForWorkFn};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::{BuildHasher, Hash};
use std::io::{Error, ErrorKind};
use std::iter::IntoIterator;

pub trait Cache {
    type Key;
    type Val;
    fn get(&mut self, key: &Self::Key) -> Option<&Self::Val>;
    fn insert(&mut self, key: Self::Key, val: Self::Val);
    fn remove(&mut self, key: &Self::Key) -> Option<Self::Val>;
    fn clear(&mut self);
}

impl<K, V, S: BuildHasher> Cache for HashMap<K, V, S>
where
    K: Eq + Hash,
{
    type Key = K;
    type Val = V;

    #[inline]
    fn get(&mut self, key: &K) -> Option<&V> {
        HashMap::get(self, key)
    }

    #[inline]
    fn insert(&mut self, key: K, val: V) {
        HashMap::insert(self, key, val);
    }

    #[inline]
    fn remove(&mut self, key: &K) -> Option<V> {
        HashMap::remove(self, key)
    }

    #[inline]
    fn clear(&mut self) {
        HashMap::clear(self)
    }
}

struct State<K, V, C = HashMap<K, V>>
where
    C: Cache<Key = K, Val = V>,
{
    completed: C,
    pending: HashSet<K>,
}

impl<K: Eq + Hash, V, C> State<K, V, C>
where
    C: Cache<Key = K, Val = V>,
{
    fn with_cache(cache: C) -> Self {
        State {
            completed: cache,
            pending: HashSet::new(),
        }
    }
}

pub struct Loader<K, V, F, C = HashMap<K, V>>
where
    K: Eq + Hash + Clone,
    V: Clone,
    F: BatchFn<K, V>,
    C: Cache<Key = K, Val = V>,
{
    state: Arc<Mutex<State<K, V, C>>>,
    load_fn: Arc<Mutex<F>>,
    wait_for_work_fn: Arc<dyn WaitForWorkFn>,
    max_batch_size: usize,
}

impl<K, V, F, C> Clone for Loader<K, V, F, C>
where
    K: Eq + Hash + Clone,
    V: Clone,
    F: BatchFn<K, V>,
    C: Cache<Key = K, Val = V>,
{
    fn clone(&self) -> Self {
        Loader {
            state: self.state.clone(),
            max_batch_size: self.max_batch_size,
            load_fn: self.load_fn.clone(),
            wait_for_work_fn: self.wait_for_work_fn.clone(),
        }
    }
}

#[allow(clippy::implicit_hasher)]
impl<K, V, F> Loader<K, V, F, HashMap<K, V>>
where
    K: Eq + Hash + Clone + Debug,
    V: Clone,
    F: BatchFn<K, V>,
{
    pub fn new(load_fn: F) -> Loader<K, V, F, HashMap<K, V>> {
        Loader::with_cache(load_fn, HashMap::new())
    }
}

impl<K, V, F, C> Loader<K, V, F, C>
where
    K: Eq + Hash + Clone + Debug,
    V: Clone,
    F: BatchFn<K, V>,
    C: Cache<Key = K, Val = V>,
{
    pub fn with_cache(load_fn: F, cache: C) -> Loader<K, V, F, C> {
        Loader {
            state: Arc::new(Mutex::new(State::with_cache(cache))),
            load_fn: Arc::new(Mutex::new(load_fn)),
            max_batch_size: 200,
            wait_for_work_fn: Arc::new(yield_fn(10)),
        }
    }

    pub fn with_max_batch_size(mut self, max_batch_size: usize) -> Self {
        self.max_batch_size = max_batch_size;
        self
    }

    pub fn with_yield_count(mut self, yield_count: usize) -> Self {
        self.wait_for_work_fn = Arc::new(yield_fn(yield_count));
        self
    }

    /// Replaces the yielding for work behavior with an arbitrary future. Rather than yielding
    /// the runtime repeatedly this will generate and `.await` a future of your choice.
    /// ***This is incompatible with*** [`Self::with_yield_count()`].
    pub fn with_custom_wait_for_work(mut self, wait_for_work_fn: impl WaitForWorkFn) {
        self.wait_for_work_fn = Arc::new(wait_for_work_fn);
    }

    pub fn max_batch_size(&self) -> usize {
        self.max_batch_size
    }

    pub async fn try_load(&self, key: K) -> Result<V, Error> {
        let mut state = self.state.lock().await;
        if let Some(v) = state.completed.get(&key) {
            return Ok((*v).clone());
        }

        if !state.pending.contains(&key) {
            state.pending.insert(key.clone());
            if state.pending.len() >= self.max_batch_size {
                let keys = state.pending.drain().collect::<Vec<K>>();
                let mut load_fn = self.load_fn.lock().await;
                let load_ret = load_fn.load(keys.as_ref()).await;
                drop(load_fn);
                for (k, v) in load_ret.into_iter() {
                    state.completed.insert(k, v);
                }
                return state.completed.get(&key).cloned().ok_or(Error::new(
                    ErrorKind::NotFound,
                    format!("could not lookup result for given key: {:?}", key),
                ));
            }
        }
        drop(state);

        (self.wait_for_work_fn)().await;

        let mut state = self.state.lock().await;
        if let Some(v) = state.completed.get(&key) {
            return Ok((*v).clone());
        }

        if !state.pending.is_empty() {
            let keys = state.pending.drain().collect::<Vec<K>>();
            let mut load_fn = self.load_fn.lock().await;
            let load_ret = load_fn.load(keys.as_ref()).await;
            drop(load_fn);
            for (k, v) in load_ret.into_iter() {
                state.completed.insert(k, v);
            }
        }

        state.completed.get(&key).cloned().ok_or(Error::new(
            ErrorKind::NotFound,
            format!("could not lookup result for given key: {:?}", key),
        ))
    }

    pub async fn load(&self, key: K) -> V {
        self.try_load(key).await.unwrap_or_else(|e| panic!("{}", e))
    }

    pub async fn try_load_many(&self, keys: Vec<K>) -> Result<HashMap<K, V>, Error> {
        let mut state = self.state.lock().await;
        let mut ret = HashMap::new();
        let mut rest = Vec::new();
        for key in keys.into_iter() {
            if let Some(v) = state.completed.get(&key).cloned() {
                ret.insert(key, v);
                continue;
            }
            if !state.pending.contains(&key) {
                state.pending.insert(key.clone());
                if state.pending.len() >= self.max_batch_size {
                    let keys = state.pending.drain().collect::<Vec<K>>();
                    let mut load_fn = self.load_fn.lock().await;
                    let load_ret = load_fn.load(keys.as_ref()).await;
                    drop(load_fn);
                    for (k, v) in load_ret.into_iter() {
                        state.completed.insert(k, v);
                    }
                }
            }
            rest.push(key);
        }
        drop(state);

        (self.wait_for_work_fn)().await;

        if !rest.is_empty() {
            let mut state = self.state.lock().await;
            if !state.pending.is_empty() {
                let keys = state.pending.drain().collect::<Vec<K>>();
                let mut load_fn = self.load_fn.lock().await;
                let load_ret = load_fn.load(keys.as_ref()).await;
                drop(load_fn);
                for (k, v) in load_ret.into_iter() {
                    state.completed.insert(k, v);
                }
            }

            for key in rest.into_iter() {
                let v = state.completed.get(&key).cloned().ok_or(Error::new(
                    ErrorKind::NotFound,
                    format!("could not lookup result for given key: {:?}", key),
                ))?;

                ret.insert(key, v);
            }
        }

        Ok(ret)
    }

    pub async fn load_many(&self, keys: Vec<K>) -> HashMap<K, V> {
        self.try_load_many(keys)
            .await
            .unwrap_or_else(|e| panic!("{}", e))
    }

    pub async fn prime(&self, key: K, val: V) {
        let mut state = self.state.lock().await;
        state.completed.insert(key, val);
    }

    pub async fn prime_many(&self, values: impl IntoIterator<Item = (K, V)>) {
        let mut state = self.state.lock().await;
        for (k, v) in values.into_iter() {
            state.completed.insert(k, v);
        }
    }

    pub async fn clear(&self, key: K) {
        let mut state = self.state.lock().await;
        state.completed.remove(&key);
    }

    pub async fn clear_all(&self) {
        let mut state = self.state.lock().await;
        state.completed.clear()
    }
}
