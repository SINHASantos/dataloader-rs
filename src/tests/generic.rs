use crate::cached::Loader;
use crate::BatchFn;
use async_std::task;
use async_trait::async_trait;
use std::collections::HashMap;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct ObjectId(usize);

#[async_trait]
trait Model {
    async fn load_many(keys: &[ObjectId]) -> HashMap<ObjectId, Result<Option<Self>, ()>>
    where
        Self: Sized;
}

#[derive(Debug, Clone)]
struct MyModel;

#[async_trait]
impl Model for MyModel {
    async fn load_many(keys: &[ObjectId]) -> HashMap<ObjectId, Result<Option<MyModel>, ()>>
    where
        Self: Sized,
    {
        keys.iter()
            .map(|k| (k.clone(), Ok(Some(MyModel))))
            .collect()
    }
}

pub struct ModelBatcher;

#[async_trait]
impl<T> BatchFn<ObjectId, Option<T>> for ModelBatcher
where
    T: Model,
{
    type Error = ();

    async fn load(&self, keys: &[ObjectId]) -> HashMap<ObjectId, Result<Option<T>, Self::Error>>
    where
        T: 'async_trait,
    {
        println!("load batch {:?}", keys);
        T::load_many(&keys).await
    }
}

#[test]
fn test_generic() {
    let loader = Loader::new(ModelBatcher);
    let f = loader.load_many(vec![ObjectId(1), ObjectId(3), ObjectId(2)]);
    let my_model: HashMap<ObjectId, Result<Option<MyModel>, ()>> = task::block_on(f);
    println!("{:?}", my_model);
}
