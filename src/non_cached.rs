use crate::BatchFn;
use async_std::sync::{Arc, Mutex};
use async_std::task;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;

type RequestId = usize;

struct State<K, V, E> {
    completed: HashMap<RequestId, Result<V, E>>,
    pending: HashMap<RequestId, K>,
    id_seq: RequestId,
}

impl<K, V, E> State<K, V, E> {
    fn new() -> Self {
        State {
            completed: HashMap::new(),
            pending: HashMap::new(),
            id_seq: 0,
        }
    }
}

pub struct Loader<K, V, E, F>
where
    K: Eq + Hash + Clone,
    V: Clone,
    E: Clone,
    F: BatchFn<K, V, Error = E>,
{
    state: Arc<Mutex<State<K, V, E>>>,
    load_fn: Arc<Mutex<F>>,
    max_batch_size: usize,
    yield_count: usize,
}

impl<K, V, E, F> Clone for Loader<K, V, E, F>
where
    K: Eq + Hash + Clone,
    V: Clone,
    E: Clone,
    F: BatchFn<K, V, Error = E>,
{
    fn clone(&self) -> Self {
        Loader {
            state: self.state.clone(),
            max_batch_size: self.max_batch_size,
            load_fn: self.load_fn.clone(),
            yield_count: self.yield_count,
        }
    }
}

impl<K, V, E, F> Loader<K, V, E, F>
where
    K: Eq + Hash + Clone + Debug,
    V: Clone,
    E: Clone,
    F: BatchFn<K, V, Error = E>,
{
    pub fn new(load_fn: F) -> Loader<K, V, E, F> {
        Loader::with_yield_count(load_fn, 10)
    }

    pub fn with_yield_count(load_fn: F, yield_count: usize) -> Loader<K, V, E, F> {
        Loader {
            state: Arc::new(Mutex::new(State::new())),
            max_batch_size: load_fn.max_batch_size(),
            load_fn: Arc::new(Mutex::new(load_fn)),
            yield_count,
        }
    }

    pub async fn load(&self, key: K) -> Result<V, F::Error> {
        let mut state = self.state.lock().await;
        let request_id = state.id_seq;
        state.id_seq = state.id_seq.wrapping_add(1);
        state.pending.insert(request_id, key);
        if state.pending.len() >= self.max_batch_size {
            let batch = state.pending.drain().collect::<HashMap<usize, K>>();
            let keys: Vec<K> = batch
                .values()
                .cloned()
                .collect::<HashSet<K>>()
                .into_iter()
                .collect();
            let load_fn = self.load_fn.lock().await;
            let load_ret = load_fn.load(keys.as_ref()).await;
            drop(load_fn);
            for (request_id, key) in batch.into_iter() {
                state.completed.insert(
                    request_id,
                    load_ret
                        .get(&key)
                        .unwrap_or_else(|| panic!("found key {:?} in load result", key))
                        .clone(),
                );
            }
            return state.completed.remove(&request_id).expect("completed");
        }
        drop(state);

        // yield for other load to append request
        let mut i = 0;
        while i < self.yield_count {
            task::yield_now().await;
            i += 1;
        }

        let mut state = self.state.lock().await;

        if state.completed.get(&request_id).is_none() {
            let batch = state.pending.drain().collect::<HashMap<usize, K>>();
            if !batch.is_empty() {
                let keys: Vec<K> = batch
                    .values()
                    .cloned()
                    .collect::<HashSet<K>>()
                    .into_iter()
                    .collect();
                let load_fn = self.load_fn.lock().await;
                let load_ret = load_fn.load(keys.as_ref()).await;
                drop(load_fn);
                for (request_id, key) in batch.into_iter() {
                    state.completed.insert(
                        request_id,
                        load_ret
                            .get(&key)
                            .unwrap_or_else(|| panic!("found key {:?} in load result", key))
                            .clone(),
                    );
                }
            }
        }
        state.completed.remove(&request_id).expect("completed")
    }

    pub async fn load_many(&self, keys: Vec<K>) -> HashMap<K, Result<V, F::Error>> {
        let mut ret = HashMap::new();
        for key in keys.into_iter() {
            let v = self.load(key.clone()).await;
            ret.insert(key, v);
        }
        ret
    }
}
