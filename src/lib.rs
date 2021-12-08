use anyhow::Context;
use blake2b_simd::Params;
use futures::{
    future::{BoxFuture, LocalBoxFuture},
    FutureExt,
    StreamExt,
};
use radixtree::radix_tree::{BlobStore as AsyncBlobStore, Tree as AsyncRadixTree, TreeStore};
use std::{convert::TryInto, sync::Arc};
use wasm_bindgen::prelude::*;

type Hash = [u8; 32];

macro_rules! console_log {
    // Note that this is using the `log` function imported above during
    // `bare_bones`
    ($($t:tt)*) => (web_sys::console::log_1(&format_args!($($t)*).to_string().into()))
}

pub struct FileStore {
    db: IdbDatabase,
}

impl FileStore {
    /// write a chunk to the file named `name`
    pub async fn append(&self, name: &str, chunk: &[u8]) -> Result<(), DomException> {
        let name_key = JsValue::from("name");
        let value_key = JsValue::from("value");
        let tx = self
            .db
            .transaction_on_multi_with_mode(&["chunks"], IdbTransactionMode::Readwrite)?;
        let chunks = tx.object_store("chunks")?;
        let val = Object::new();
        Reflect::set(&val, &name_key, &name.into())?;
        Reflect::set(&val, &value_key, &Uint8Array::from(chunk))?;
        chunks.put_val_owned(val)?;
        tx.await.into_result()?;
        Ok(())
    }

    /// move from file from to file to, deleting to if it already exists
    pub async fn mv(&self, from: &str, to: &str) -> Result<(), DomException> {
        let tx = self
            .db
            .transaction_on_multi_with_mode(&["chunks"], IdbTransactionMode::Readwrite)?;
        let chunks = tx.object_store("chunks")?;
        let index = chunks.index("name_idx")?;
        // delete "to" file, if it exists
        let range = IdbKeyRange::only(&JsValue::from(to))?;
        if let Some(cursor) = index.open_cursor_with_range_owned(range)?.await? {
            while {
                cursor.delete()?;
                cursor.continue_cursor()?.await?
            } {}
        }
        // copy the chunks over, one by one
        let range = IdbKeyRange::only(&JsValue::from(from))?;
        if let Some(cursor) = index.open_cursor_with_range_owned(range)?.await? {
            let name_key = JsValue::from("name");
            let value_key = JsValue::from("value");
            while {
                let value = cursor.value();
                let data = Reflect::get(&value, &value_key)?;
                let val = Object::new();
                Reflect::set(&val, &name_key, &to.into())?;
                Reflect::set(&val, &value_key, &data)?;
                chunks.put_val_owned(val)?;
                cursor.delete()?;
                cursor.continue_cursor()?.await?
            } {}
        }
        tx.await.into_result()?;
        Ok(())
    }

    /// get all chunks with `name`, in insertion order
    pub async fn load(
        &self,
        name: &str,
        mut f: Box<dyn FnMut(&[u8]) + '_>,
    ) -> Result<(), DomException> {
        let tx = self
            .db
            .transaction_on_multi_with_mode(&["chunks"], IdbTransactionMode::Readwrite)?;
        let chunks = tx.object_store("chunks")?;
        let index = chunks.index("name_idx")?;
        let all: Array = index.get_all_with_key_owned(JsValue::from(name))?.await?;
        let mut res = Vec::new();
        for x in all.iter() {
            let m = Reflect::get(&x, &JsValue::from("value"))?;
            let m = Uint8Array::from(m);
            let s = res.len();
            res.extend(std::iter::repeat(0u8).take(m.length() as usize));
            m.copy_to(&mut res[s..]);
        }
        f(res.as_slice());
        Ok(())
    }

    pub async fn new(name: &str) -> Result<Self, DomException> {
        // Open my_db v1
        let mut open_req = IdbDatabase::open_u32(name, 1)?;
        open_req.set_on_upgrade_needed(Some(
            |evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
                if let None = evt.db().object_store_names().find(|n| n == "chunks") {
                    let mut params = IdbObjectStoreParameters::new();
                    params.key_path(Some(&"id".into()));
                    params.auto_increment(true);
                    let store = evt
                        .db()
                        .create_object_store_with_params("chunks", &params)?;
                    store.create_index_with_params(
                        "name_idx",
                        &IdbKeyPath::str("name"),
                        &IdbIndexParameters::new().unique(false),
                    )?;
                }
                Ok(())
            },
        ));
        let db = open_req.into_future().await?;
        Ok(Self { db })
    }
}

#[derive(Debug)]
pub struct BlobStore {
    db: IdbDatabase,
}

unsafe impl Sync for BlobStore {}
unsafe impl Send for BlobStore {}

impl AsyncBlobStore for BlobStore {
    fn load(&self, hash: Hash) -> BoxFuture<'_, anyhow::Result<Arc<[u8]>>> {
        console_log!("load {}", hex::encode(&hash));
        let future: LocalBoxFuture<'_, anyhow::Result<Arc<[u8]>>> = async move {
            let mut tx = self.tx().map_err(|e| anyhow::anyhow!("tx error {:?}", e))?;
            let res = tx
                .get(&hash)
                .await
                .map_err(|e| anyhow::anyhow!("get error {:?}", e))?;
            let res = res.context("value not found")?;
            Ok(res.into())
        }
        .boxed_local();
        unsafe { std::mem::transmute(future) }
    }

    fn store(&self, _hash: Hash, value: &[u8]) -> BoxFuture<'_, anyhow::Result<()>> {
        let future: LocalBoxFuture<'_, anyhow::Result<()>> = async move {
            let mut tx = self.tx().map_err(|e| anyhow::anyhow!("tx error {:?}", e))?;
            tx.put(value)
                .map_err(|e| anyhow::anyhow!("put error {:?}", e))?;
            tx.commit()
                .await
                .map_err(|e| anyhow::anyhow!("commit error {:?}", e))?;
            Ok(())
        }
        .boxed_local();
        unsafe { std::mem::transmute(future) }
    }
}

pub struct Transaction<'a> {
    tx: Box<IdbTransaction<'a>>,
    blobs: IdbObjectStore<'a>,
}

impl<'a> Transaction<'a> {
    pub fn put(&mut self, data: &[u8]) -> Result<Hash, DomException> {
        let hash = Params::new()
            .hash_length(32)
            .to_state()
            .update(&data)
            .finalize();
        let key = Uint8Array::from(hash.as_bytes());
        let value = Uint8Array::from(data);
        // let key = JsValue::from(hex::encode(hash.as_bytes()));
        // let value = JsValue::from(hex::encode(data));
        console_log!("put key={} value={}", hex::encode(hash.as_bytes()), hex::encode(data));
        self.blobs.put_key_val(&key, &value)?;
        let hash = hash.as_bytes().try_into().expect("hash should be 32 bytes");
        Ok(hash)
    }

    pub async fn get(&mut self, hash: &Hash) -> Result<Option<Vec<u8>>, DomException> {
        let key = Uint8Array::from(&hash[..]);
        // let key = JsValue::from(hex::encode(&hash[..]));        
        console_log!("get key={}", hex::encode(&hash[..]));
        Ok(match self.blobs.get_owned(key)?.await? {
            Some(data) => {
                let data: Uint8Array = data.into();
                let data = data.to_vec();
                console_log!("got {}", hex::encode(&data));
                Some(data)
            }
            None => {
                console_log!("got None");
                None
            }
        })
    }

    pub async fn commit(self) -> Result<(), DomException> {
        self.tx.await.into_result()
    }
}

impl BlobStore {
    pub async fn new(name: &str) -> Result<Self, DomException> {
        // Open my_db v1
        let mut open_req = IdbDatabase::open_u32(name, 1)?;
        open_req.set_on_upgrade_needed(Some(
            |evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
                // Check if the object store exists; create it if it doesn't
                if let None = evt.db().object_store_names().find(|n| n == "blobs") {
                    evt.db().create_object_store("blobs")?;
                }
                Ok(())
            },
        ));
        let db = open_req.into_future().await?;
        Ok(Self { db })
    }

    pub fn tx<'a>(&'a self) -> Result<Transaction<'a>, DomException> {
        // put tx in a box so it does not get moved after creation
        let tx = Box::new(
            self.db
                .transaction_on_one_with_mode("blobs", IdbTransactionMode::Readwrite)?,
        );
        // get the object store and ignore lifetime issues
        let blobs = unsafe { std::mem::transmute(tx.object_store("blobs")?) };
        Ok(Transaction { tx, blobs })
    }
}

#[wasm_bindgen(start)]
pub async fn run() -> Result<(), DomException> {
    let blobs = Arc::new(BlobStore::new("tlfs2").await?);
    let trees = TreeStore::new(blobs.clone());
    let mut tree = AsyncRadixTree::empty();    
    for i in 0..10000 {
        console_log!("{}", i);
        let k: Arc<[u8]> = i.to_string().as_bytes().to_vec().into();
        tree.union_with(&AsyncRadixTree::single(k.clone(), k), &trees.reader)
            .await
            .map_err(|e| DomException::new().unwrap())?;
    }
    console_log!("{:?}", tree);
    tree.shrink(&trees.writer).await.map_err(|e| DomException::new().unwrap())?;
    console_log!("{:?}", tree);

    let v = blobs.load([57, 243, 64, 152, 186, 109, 72, 192, 164, 143, 196, 223, 130, 233, 170, 107, 167, 190, 14, 74, 249, 169, 155, 175, 121, 164, 58, 122, 215, 198, 104, 161]).await;
    console_log!("manual1 {:?}", v);
    let v = trees.reader.load([57, 243, 64, 152, 186, 109, 72, 192, 164, 143, 196, 223, 130, 233, 170, 107, 167, 190, 14, 74, 249, 169, 155, 175, 121, 164, 58, 122, 215, 198, 104, 161]).await;
    console_log!("manual2 {:?}", v);
    tree.ensure_data(&trees.reader).await.map_err(|e| DomException::new().unwrap())?;
    console_log!("manual3 {:?}", tree);

    let mut stream = tree.stream(&trees.reader);
    console_log!("streaming tree");
    stream.next().await;
    console_log!("streaming tree");    
    while let Some(Ok((k, v))) = stream.next().await {
        console_log!("{}=>{}", hex::encode(k), hex::encode(v));
    }
    console_log!("done");
    Ok(())
}

use indexed_db_futures::{
    js_sys::{Array, Object, Reflect, Uint8Array},
    prelude::*,
};
use web_sys::{DomException, IdbKeyRange};

pub async fn example() -> Result<(), DomException> {
    use web_sys::console;

    // Open db v1
    let mut db_req: OpenDbRequest = IdbDatabase::open_u32("my_db", 2)?;
    db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        // Check if the object store exists; create it if it doesn't
        if let None = evt.db().object_store_names().find(|n| n == "blobs") {
            evt.db().create_object_store("blobs")?;
        }
        Ok(())
    }));
    let db: IdbDatabase = db_req.into_future().await?;

    // Insert/overwrite a record
    let tx: IdbTransaction =
        db.transaction_on_one_with_mode("blobs", IdbTransactionMode::Readwrite)?;
    let store: IdbObjectStore = tx.object_store("blobs")?;

    let value_to_put: JsValue = 1.into();
    store.put_key_val_owned("my_key", &value_to_put)?;

    // IDBTransactions can have an Error or an Abort event; into_result() turns both into a
    // DOMException
    tx.await.into_result()?;

    // // Delete a record
    // let tx = db.transaction_on_one_with_mode("my_store", IdbTransactionMode::Readwrite)?;
    // let store = tx.object_store("my_store")?;
    // store.delete_owned("my_key")?;
    // tx.await.into_result()?;

    // Get a record
    let tx = db.transaction_on_one("blobs")?;
    let store = tx.object_store("blobs")?;

    let value: Option<JsValue> = store.get_owned("my_key")?.await?;
    for value in value {
        console::log_2(&"Logging arbitrary values looks like".into(), &value);
    }

    // All of the requests in the transaction have already finished so we can just drop it to
    // avoid the unused future warning, or assign it to _.
    let _ = tx;

    Ok(())
}
