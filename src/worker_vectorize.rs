// worker_vectorize.rs — Hand-rolled Vectorize binding for the wasm32 worker.
//
// workers-rs 0.8.3 doesn't ship a Vectorize wrapper (Cloudflare's Rust SDK
// is behind their JS SDK on this binding). We do it ourselves via
// `#[wasm_bindgen] extern` to declare the JS methods, plus an
// `EnvBinding` impl with an overridden `get()` that skips the runtime
// constructor-name check (we don't know what the V8 isolate calls the
// VectorizeIndex class internally, and it varies between Cloudflare
// releases — bypassing the check keeps this resilient to that).
//
// The JS API we're wrapping:
//
//   await env.VECTORS.upsert([{id, values, metadata?}]) →
//       { mutationId, count }
//
//   await env.VECTORS.query(vector, { topK, returnValues?, ... }) →
//       { count, matches: [{id, score, values?, metadata?}] }
//
// Higher-level functions `upsert` and `query_top_k` wrap the raw FFI
// with idiomatic Rust signatures.

use js_sys::{Array, Object, Promise, Reflect};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use worker::{Env, EnvBinding, Error, Result};

#[wasm_bindgen]
extern "C" {
    /// Opaque handle to a Vectorize index binding. Methods are wired
    /// via `#[wasm_bindgen(method, catch)]` below.
    pub type VectorizeIndex;

    /// upsert(vectors: VectorizeVector[]) → Promise<VectorizeAsyncMutation>
    #[wasm_bindgen(method, catch)]
    fn upsert(this: &VectorizeIndex, vectors: JsValue) -> Result<Promise, JsValue>;

    /// query(vector: number[] | Float32Array, options?: VectorizeQueryOptions)
    ///     → Promise<VectorizeMatches>
    #[wasm_bindgen(method, catch)]
    fn query(this: &VectorizeIndex, vector: JsValue, options: JsValue)
        -> Result<Promise, JsValue>;

    /// deleteByIds(ids: string[]) → Promise<VectorizeAsyncMutation>
    #[wasm_bindgen(method, js_name = "deleteByIds", catch)]
    fn delete_by_ids(this: &VectorizeIndex, ids: JsValue) -> Result<Promise, JsValue>;

    /// getByIds(ids: string[]) → Promise<VectorizeVector[]>
    /// Returns each id's stored vector (no score — there's no query to
    /// score against). Used by hybrid retrieval (CLA-109) to fetch
    /// vectors for ids that FTS surfaced but Vectorize's top-K query
    /// didn't include — so those candidates can still participate in
    /// MMR diversity ranking.
    #[wasm_bindgen(method, js_name = "getByIds", catch)]
    fn get_by_ids(this: &VectorizeIndex, ids: JsValue) -> Result<Promise, JsValue>;
}

impl EnvBinding for VectorizeIndex {
    // The actual JS constructor name (varies; bypassed in `get`).
    const TYPE_NAME: &'static str = "VectorizeIndex";

    /// Skip the default constructor-name check — we trust the binding
    /// name from wrangler.toml to indicate the right shape. The JS
    /// class name has changed across Cloudflare runtime updates and
    /// pinning to a string here is brittle.
    fn get(val: JsValue) -> Result<Self> {
        Ok(val.unchecked_into())
    }
}

/// One hit from a `query()` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorMatch {
    /// The vector's id — same as the memory_id we use in D1.
    pub id: String,
    /// Cosine similarity score (higher = closer in vector space).
    pub score: f64,
}

/// One hit from a `query()` call made with `returnValues: true`. Used by
/// the MMR reranking path, which needs each candidate's vector to
/// compute pairwise candidate-candidate similarity for the diversity term.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorMatchWithVector {
    pub id: String,
    pub score: f64,
    pub values: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct QueryResponse {
    #[serde(default)]
    matches: Vec<VectorMatch>,
}

#[derive(Debug, Deserialize)]
struct QueryResponseWithVectors {
    #[serde(default)]
    matches: Vec<VectorMatchWithVector>,
}

/// getByIds returns bare `{id, values}` objects — no score (there was no
/// query). Used by hybrid retrieval to pull vectors for FTS-only hits.
#[derive(Debug, Deserialize)]
pub struct StoredVector {
    pub id: String,
    #[serde(default)]
    pub values: Vec<f64>,
}

fn from_env(env: &Env, binding_name: &str) -> Result<VectorizeIndex> {
    env.get_binding::<VectorizeIndex>(binding_name)
}

fn js_err(e: JsValue) -> Error {
    Error::JsError(format!("{:?}", e))
}

/// Insert or update a single vector. `id` becomes the lookup key (use
/// the memory_id so D1 lookups can correlate).
pub async fn upsert_one(env: &Env, id: &str, values: &[f64]) -> Result<()> {
    let index = from_env(env, "VECTORS")?;

    // Build { id, values: [...] }
    let vector_obj = Object::new();
    Reflect::set(&vector_obj, &"id".into(), &id.into()).map_err(js_err)?;
    let values_arr: Array = values.iter().copied().map(JsValue::from_f64).collect();
    Reflect::set(&vector_obj, &"values".into(), &values_arr.into()).map_err(js_err)?;

    // Wrap in an array — upsert takes a batch.
    let batch = Array::new();
    batch.push(&vector_obj);

    let promise = index.upsert(batch.into()).map_err(js_err)?;
    JsFuture::from(promise).await.map_err(js_err)?;
    Ok(())
}

/// Remove vectors by id. Used when a memory is forgotten — keeps the
/// vector index in sync with D1 so semantic recall doesn't surface
/// stale ids that no longer resolve.
pub async fn delete_ids(env: &Env, ids: &[&str]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let index = from_env(env, "VECTORS")?;
    let arr: Array = ids.iter().map(|s| JsValue::from_str(s)).collect();
    let promise = index.delete_by_ids(arr.into()).map_err(js_err)?;
    JsFuture::from(promise).await.map_err(js_err)?;
    Ok(())
}

/// Top-k similarity search. Returns matches sorted by score descending.
pub async fn query_top_k(env: &Env, vector: &[f64], top_k: u32) -> Result<Vec<VectorMatch>> {
    let index = from_env(env, "VECTORS")?;

    let vector_arr: Array = vector.iter().copied().map(JsValue::from_f64).collect();
    let options = Object::new();
    Reflect::set(&options, &"topK".into(), &(top_k as f64).into()).map_err(js_err)?;

    let promise = index
        .query(vector_arr.into(), options.into())
        .map_err(js_err)?;
    let result = JsFuture::from(promise).await.map_err(js_err)?;

    let response: QueryResponse = serde_wasm_bindgen::from_value(result)
        .map_err(|e| Error::JsError(format!("Vectorize response parse failed: {}", e)))?;
    Ok(response.matches)
}

/// Top-k similarity search that also returns each match's embedding
/// vector (Vectorize's `returnValues: true` option). Used by the MMR
/// reranker, which needs candidate vectors to compute pairwise
/// candidate-candidate similarity for the diversity term.
pub async fn query_top_k_with_vectors(
    env: &Env,
    vector: &[f64],
    top_k: u32,
) -> Result<Vec<VectorMatchWithVector>> {
    let index = from_env(env, "VECTORS")?;

    let vector_arr: Array = vector.iter().copied().map(JsValue::from_f64).collect();
    let options = Object::new();
    Reflect::set(&options, &"topK".into(), &(top_k as f64).into()).map_err(js_err)?;
    Reflect::set(&options, &"returnValues".into(), &JsValue::TRUE).map_err(js_err)?;

    let promise = index
        .query(vector_arr.into(), options.into())
        .map_err(js_err)?;
    let result = JsFuture::from(promise).await.map_err(js_err)?;

    let response: QueryResponseWithVectors = serde_wasm_bindgen::from_value(result)
        .map_err(|e| Error::JsError(format!("Vectorize response parse failed: {}", e)))?;
    Ok(response.matches)
}

/// Fetch stored vectors by id — no query, no scoring, just a lookup.
/// Used by hybrid retrieval (CLA-109): when FTS surfaces an id that
/// wasn't in Vectorize's top-K, we still want it to participate in MMR
/// diversity ranking, which needs the vector.
///
/// Ids not present in the index are silently dropped from the returned
/// list (Cloudflare's behaviour, not ours). Caller should not assume
/// the response length matches the request length.
pub async fn get_by_ids(env: &Env, ids: &[&str]) -> Result<Vec<StoredVector>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let index = from_env(env, "VECTORS")?;
    let ids_arr: Array = ids.iter().map(|s| JsValue::from_str(s)).collect();
    let promise = index.get_by_ids(ids_arr.into()).map_err(js_err)?;
    let result = JsFuture::from(promise).await.map_err(js_err)?;
    // getByIds returns a bare array, not a `{matches: [...]}` envelope.
    let vectors: Vec<StoredVector> = serde_wasm_bindgen::from_value(result)
        .map_err(|e| Error::JsError(format!("Vectorize getByIds parse failed: {}", e)))?;
    Ok(vectors)
}
