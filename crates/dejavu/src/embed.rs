//! Embedding providers for semantic memory search. Any OpenAI-compatible
//! `/embeddings` endpoint works; Gemini's native API adds the half-price Batch
//! API for indexing. Nothing here runs unless the user stores an API key.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::js;

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";
pub const DEFAULT_MODEL: &str = "gemini-embedding-2";
pub const DEFAULT_DIMS: usize = 768;
/// Texts per standard request; Gemini's `batchEmbedContents` accepts 100.
const STANDARD_GROUP: usize = 100;
const GEMINI_HOST: &str = "generativelanguage.googleapis.com";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Api {
    OpenAi,
    Gemini,
}

#[derive(Debug, Clone)]
pub struct EmbedConfig {
    pub base_url: String,
    /// One key, or a pool: a rate-limited standard request moves to the next.
    pub keys: Vec<String>,
    pub model: String,
    pub dims: usize,
    pub api: Api,
    /// Index through the provider's batch API when it has one.
    pub batch: bool,
}

/// A failed provider call. `retryable` is false when repeating the same
/// request cannot succeed (a rejected input, a bad key, an unsupported call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedError {
    pub retryable: bool,
    /// The HTTP status, when the provider answered.
    pub status: Option<u16>,
    pub message: String,
}

impl EmbedError {
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            retryable: false,
            status: None,
            message: message.into(),
        }
    }
    fn transient(message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            status: None,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BatchPoll {
    Running,
    /// One entry per submitted text, in submission order.
    Done(Vec<Result<Vec<f32>, String>>),
    Failed(String),
}

pub trait Embedder {
    fn model(&self) -> &str;
    fn dims(&self) -> usize;
    /// One unit-length vector per text, through the standard endpoint.
    fn embed(&self, texts: &[String], timeout: Duration) -> Result<Vec<Vec<f32>>, EmbedError>;
    fn batch_supported(&self) -> bool;
    /// Queues texts on the batch API and returns the batch name.
    fn submit_batch(&self, texts: &[String]) -> Result<String, EmbedError>;
    fn poll_batch(&self, name: &str) -> Result<BatchPoll, EmbedError>;
    /// Best effort; a batch that cannot be cancelled is left to finish.
    fn cancel_batch(&self, name: &str);
}

pub fn key_file_path() -> PathBuf {
    if let Some(path) = std::env::var_os("DEJAVU_EMBED_KEY_FILE").filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(crate::sources::home_dir()).join(".config"))
        .join("dejavu/embed-key")
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The lines of a key file that are not blank or `#` comments, one key each.
fn keys_from_file(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// `None` when no key is stored: semantic search is opt-in and stays off.
/// The key comes from `DEJAVU_EMBED_KEY`, else the key file. Other keys in the
/// environment (`GEMINI_API_KEY`, `OPENAI_API_KEY`) are never used, so memory
/// leaves the machine only after an explicit setup step.
pub fn config_from_env() -> Result<Option<EmbedConfig>, String> {
    let keys = match env("DEJAVU_EMBED_KEY") {
        Some(key) => vec![key],
        None => std::fs::read_to_string(key_file_path())
            .map(|text| keys_from_file(&text))
            .unwrap_or_default(),
    };
    if keys.is_empty() {
        return Ok(None);
    }
    let base_url = env("DEJAVU_EMBED_BASE_URL")
        .unwrap_or_else(|| DEFAULT_BASE_URL.into())
        .trim_end_matches('/')
        .to_string();
    let dims = match env("DEJAVU_EMBED_DIMS") {
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|dims| (1..=8192).contains(dims))
            .ok_or_else(|| format!("DEJAVU_EMBED_DIMS must be an integer 1-8192 (got '{raw}')"))?,
        None => DEFAULT_DIMS,
    };
    let api = match env("DEJAVU_EMBED_API").as_deref() {
        Some("openai") => Api::OpenAi,
        Some("gemini") => Api::Gemini,
        Some(other) => {
            return Err(format!(
                "DEJAVU_EMBED_API must be openai or gemini (got '{other}')"
            ));
        }
        None if base_url.contains(GEMINI_HOST) => Api::Gemini,
        None => Api::OpenAi,
    };
    Ok(Some(EmbedConfig {
        base_url,
        keys,
        model: env("DEJAVU_EMBED_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into()),
        dims,
        api,
        batch: env("DEJAVU_EMBED_BATCH").as_deref() != Some("0"),
    }))
}

/// Gemini Embedding 2 takes its task as a text prefix; other models get the
/// plain query.
pub fn format_query(model: &str, query: &str) -> String {
    if model.starts_with("gemini-embedding-2") {
        format!("task: search result | query: {query}")
    } else {
        query.to_string()
    }
}

pub fn format_document(model: &str, title: &str, text: &str) -> String {
    if model.starts_with("gemini-embedding-2") {
        let title = if title.is_empty() { "none" } else { title };
        format!("title: {title} | text: {text}")
    } else if title.is_empty() {
        text.to_string()
    } else {
        format!("{title}\n\n{text}")
    }
}

/// Scales a vector to unit length, so a dot product is the cosine similarity.
pub fn normalize(vector: &mut [f32]) {
    let norm = vector
        .iter()
        .map(|v| f64::from(*v) * f64::from(*v))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for value in vector.iter_mut() {
            *value = (f64::from(*value) / norm) as f32;
        }
    }
}

fn vector_from(values: Option<&Value>, dims: usize) -> Result<Vec<f32>, String> {
    let Some(values) = values.and_then(Value::as_array) else {
        return Err("response has no embedding values".into());
    };
    if values.len() != dims {
        return Err(format!(
            "the model returned {} dimensions, expected {dims} (set DEJAVU_EMBED_DIMS)",
            values.len()
        ));
    }
    let mut vector: Vec<f32> = values
        .iter()
        .map(|value| value.as_f64().unwrap_or(0.0) as f32)
        .collect();
    normalize(&mut vector);
    Ok(vector)
}

/// The provider over HTTPS.
pub struct HttpEmbedder {
    config: EmbedConfig,
    /// The pool key standard requests currently use.
    current: std::cell::Cell<usize>,
    /// Pool keys the provider refused in this run.
    refused: std::cell::RefCell<Vec<bool>>,
}

impl HttpEmbedder {
    pub fn new(config: EmbedConfig) -> Self {
        Self {
            refused: std::cell::RefCell::new(vec![false; config.keys.len()]),
            config,
            current: std::cell::Cell::new(0),
        }
    }

    /// `https://…/v1beta` for the native Gemini calls.
    fn gemini_root(&self) -> &str {
        self.config
            .base_url
            .strip_suffix("/openai")
            .unwrap_or(&self.config.base_url)
    }

    fn send(
        &self,
        method: &str,
        url: &str,
        body: Option<&Value>,
        timeout: Duration,
        key: &str,
    ) -> Result<Value, EmbedError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build()
            .into();
        let auth = match self.config.api {
            Api::Gemini => ("x-goog-api-key", key.to_string()),
            Api::OpenAi => ("Authorization", format!("Bearer {key}")),
        };
        let sent = match (method, body) {
            ("GET", _) => agent.get(url).header(auth.0, &auth.1).call(),
            (_, body) => agent
                .post(url)
                .header(auth.0, &auth.1)
                .header("Content-Type", "application/json")
                .send(body.map(Value::to_string).unwrap_or_else(|| "{}".into())),
        };
        let mut response = sent
            .map_err(|error| EmbedError::transient(format!("embedding request failed: {error}")))?;
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().map_err(|error| {
            EmbedError::transient(format!("embedding response unreadable: {error}"))
        })?;
        if status != 200 {
            return Err(EmbedError {
                retryable: status == 408 || status == 429 || status >= 500,
                status: Some(status),
                message: format!(
                    "embedding provider returned HTTP {status}: {}",
                    js::slice(&text, 0, 300)
                ),
            });
        }
        serde_json::from_str(&text)
            .map_err(|error| EmbedError::fatal(format!("embedding response is not JSON: {error}")))
    }

    /// A standard request, starting at the current pool key and skipping the
    /// keys the provider refused (401, 403) earlier in this run. A
    /// rate-limited key (HTTP 429) hands over to the next one. When no key
    /// answers, a rate limit is reported in preference to a refusal, so the
    /// caller waits and retries while a usable key remains.
    fn send_pooled(&self, url: &str, body: &Value, timeout: Duration) -> Result<Value, EmbedError> {
        let keys = &self.config.keys;
        let start = self.current.get();
        let (mut limited, mut refused) = (None, None);
        for index in (0..keys.len()).map(|step| (start + step) % keys.len()) {
            if self.refused.borrow()[index] {
                continue;
            }
            let outcome = self.send("POST", url, Some(body), timeout, &keys[index]);
            match outcome.as_ref().err().and_then(|error| error.status) {
                Some(401 | 403) if keys.len() > 1 => {
                    self.refused.borrow_mut()[index] = true;
                    refused = outcome.err();
                }
                Some(429) => limited = outcome.err(),
                _ => {
                    self.current.set(index);
                    return outcome;
                }
            }
        }
        Err(limited
            .or(refused)
            .unwrap_or_else(|| EmbedError::fatal("the provider refused every embedding key")))
    }

    /// Batch jobs belong to the key that created them: always the first.
    fn send_batch(
        &self,
        method: &str,
        url: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, EmbedError> {
        self.send(method, url, body, timeout, &self.config.keys[0])
    }

    fn gemini_request(&self, text: &str) -> Value {
        json!({
            "model": format!("models/{}", self.config.model),
            "content": {"parts": [{"text": text}]},
            "outputDimensionality": self.config.dims,
        })
    }

    fn embed_group(
        &self,
        texts: &[String],
        timeout: Duration,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let dims = self.config.dims;
        let vectors: Vec<Result<Vec<f32>, String>> = match self.config.api {
            Api::OpenAi => {
                let url = format!("{}/embeddings", self.config.base_url);
                let body = json!({
                    "model": self.config.model,
                    "input": texts,
                    "dimensions": dims,
                    "encoding_format": "float",
                });
                let response = self.send_pooled(&url, &body, timeout)?;
                let mut rows: Vec<&Value> = response
                    .get("data")
                    .and_then(Value::as_array)
                    .map(|rows| rows.iter().collect())
                    .unwrap_or_default();
                rows.sort_by_key(|row| row.get("index").and_then(Value::as_u64).unwrap_or(0));
                rows.into_iter()
                    .map(|row| vector_from(row.get("embedding"), dims))
                    .collect()
            }
            Api::Gemini if texts.len() == 1 => {
                let url = format!(
                    "{}/models/{}:embedContent",
                    self.gemini_root(),
                    self.config.model
                );
                let body = self.gemini_request(&texts[0]);
                let response = self.send_pooled(&url, &body, timeout)?;
                vec![vector_from(response.pointer("/embedding/values"), dims)]
            }
            Api::Gemini => {
                let url = format!(
                    "{}/models/{}:batchEmbedContents",
                    self.gemini_root(),
                    self.config.model
                );
                let requests: Vec<Value> =
                    texts.iter().map(|text| self.gemini_request(text)).collect();
                let response = self.send_pooled(&url, &json!({"requests": requests}), timeout)?;
                response
                    .get("embeddings")
                    .and_then(Value::as_array)
                    .map(|rows| {
                        rows.iter()
                            .map(|row| vector_from(row.get("values"), dims))
                            .collect()
                    })
                    .unwrap_or_default()
            }
        };
        if vectors.len() != texts.len() {
            return Err(EmbedError::fatal(format!(
                "embedding provider returned {} vectors for {} texts",
                vectors.len(),
                texts.len()
            )));
        }
        vectors
            .into_iter()
            .collect::<Result<Vec<_>, String>>()
            .map_err(EmbedError::fatal)
    }
}

/// The inline responses of a finished Gemini embedding batch. The REST
/// reference nests them under `response.output`; the guide's example reads
/// `response.inlinedResponses`. Both wrap the array in a same-named object.
fn inlined_responses(operation: &Value) -> Option<&Vec<Value>> {
    let response = operation.get("response")?;
    let holder = response
        .pointer("/output/inlinedResponses")
        .or_else(|| response.get("inlinedResponses"))?;
    holder.get("inlinedResponses").unwrap_or(holder).as_array()
}

fn read_batch(operation: &Value, dims: usize) -> BatchPoll {
    let state = operation
        .pointer("/metadata/state")
        .and_then(Value::as_str)
        .unwrap_or("");
    let done = operation.get("done").and_then(Value::as_bool) == Some(true);
    match state {
        "JOB_STATE_FAILED"
        | "JOB_STATE_CANCELLED"
        | "JOB_STATE_EXPIRED"
        | "BATCH_STATE_FAILED"
        | "BATCH_STATE_CANCELLED"
        | "BATCH_STATE_EXPIRED" => {
            return BatchPoll::Failed(format!(
                "{state}: {}",
                operation
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("no detail")
            ));
        }
        _ if !done => return BatchPoll::Running,
        _ => {}
    }
    if let Some(message) = operation.pointer("/error/message").and_then(Value::as_str) {
        return BatchPoll::Failed(message.to_string());
    }
    match inlined_responses(operation) {
        Some(rows) => BatchPoll::Done(
            rows.iter()
                .map(|row| match row.pointer("/error/message") {
                    Some(message) => Err(message.as_str().unwrap_or("request failed").to_string()),
                    None => vector_from(row.pointer("/response/embedding/values"), dims),
                })
                .collect(),
        ),
        None => BatchPoll::Failed("the finished batch has no inline responses".into()),
    }
}

const BATCH_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_TIMEOUT: Duration = Duration::from_secs(20);

impl Embedder for HttpEmbedder {
    fn model(&self) -> &str {
        &self.config.model
    }

    fn dims(&self) -> usize {
        self.config.dims
    }

    fn embed(&self, texts: &[String], timeout: Duration) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut vectors = Vec::with_capacity(texts.len());
        for group in texts.chunks(STANDARD_GROUP) {
            vectors.extend(self.embed_group(group, timeout)?);
        }
        Ok(vectors)
    }

    fn batch_supported(&self) -> bool {
        self.config.batch && self.config.api == Api::Gemini
    }

    fn submit_batch(&self, texts: &[String]) -> Result<String, EmbedError> {
        let url = format!(
            "{}/models/{}:asyncBatchEmbedContent",
            self.gemini_root(),
            self.config.model
        );
        let requests: Vec<Value> = texts
            .iter()
            .enumerate()
            .map(|(index, text)| {
                json!({"request": self.gemini_request(text), "metadata": {"key": index.to_string()}})
            })
            .collect();
        let body = json!({"batch": {
            "displayName": "dejavu-memory",
            "inputConfig": {"requests": {"requests": requests}},
        }});
        let response = self.send_batch("POST", &url, Some(&body), BATCH_TIMEOUT)?;
        response
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .ok_or_else(|| EmbedError::fatal("the batch API returned no batch name"))
    }

    fn poll_batch(&self, name: &str) -> Result<BatchPoll, EmbedError> {
        let url = format!("{}/{name}", self.gemini_root());
        let operation = self.send_batch("GET", &url, None, POLL_TIMEOUT)?;
        Ok(read_batch(&operation, self.config.dims))
    }

    fn cancel_batch(&self, name: &str) {
        let url = format!("{}/{name}:cancel", self.gemini_root());
        let _ = self.send_batch("POST", &url, None, POLL_TIMEOUT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_files_hold_one_key_per_line_without_blank_lines_and_comments() {
        assert_eq!(
            keys_from_file("\n# pool\n  first-key  \nsecond-key\n"),
            ["first-key", "second-key"]
        );
        assert!(keys_from_file("# only a comment\n").is_empty());
    }

    #[test]
    fn gemini_embedding_2_gets_task_prefixes_and_other_models_plain_text() {
        assert_eq!(
            format_query("gemini-embedding-2", "disk full"),
            "task: search result | query: disk full"
        );
        assert_eq!(
            format_document("gemini-embedding-2-preview", "", "body"),
            "title: none | text: body"
        );
        assert_eq!(
            format_query("text-embedding-3-small", "disk full"),
            "disk full"
        );
        assert_eq!(
            format_document("text-embedding-3-small", "note", "body"),
            "note\n\nbody"
        );
    }

    #[test]
    fn vectors_are_normalized_and_wrong_sizes_rejected() {
        let vector = vector_from(Some(&json!([3.0, 4.0])), 2).unwrap();
        assert!((vector[0] - 0.6).abs() < 1e-6 && (vector[1] - 0.8).abs() < 1e-6);
        assert!(
            vector_from(Some(&json!([1.0, 2.0, 3.0])), 2)
                .unwrap_err()
                .contains("expected 2")
        );
    }

    #[test]
    fn a_batch_reads_as_running_failed_or_done_in_either_documented_shape() {
        assert_eq!(
            read_batch(&json!({"metadata": {"state": "JOB_STATE_RUNNING"}}), 2),
            BatchPoll::Running
        );
        assert!(matches!(
            read_batch(
                &json!({"done": true, "metadata": {"state": "JOB_STATE_EXPIRED"}}),
                2
            ),
            BatchPoll::Failed(_)
        ));
        let rows = json!([
            {"response": {"embedding": {"values": [1.0, 0.0]}}},
            {"error": {"message": "too long"}},
        ]);
        let expected = BatchPoll::Done(vec![Ok(vec![1.0, 0.0]), Err("too long".into())]);
        let nested = json!({"done": true, "metadata": {"state": "JOB_STATE_SUCCEEDED"},
            "response": {"output": {"inlinedResponses": {"inlinedResponses": rows}}}});
        assert_eq!(read_batch(&nested, 2), expected);
        let flat = json!({"done": true, "metadata": {"state": "JOB_STATE_SUCCEEDED"},
            "response": {"inlinedResponses": {"inlinedResponses": rows}}});
        assert_eq!(read_batch(&flat, 2), expected);
    }

    /// A localhost OpenAI-format endpoint that answers by key: `limited`
    /// gets HTTP 429, `refused` 403, any other key a vector.
    #[test]
    fn a_rate_limited_pool_key_hands_over_and_a_refused_one_leaves_the_pool() {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        let recovered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = recovered.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut length, mut key) = (0, String::new());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if let Some(value) = lower.strip_prefix("authorization: bearer ") {
                        key = value.trim().to_string();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let limited = key == "limited"
                    || (key == "recovers" && !flag.load(std::sync::atomic::Ordering::SeqCst));
                let (status, reply) = match key.as_str() {
                    _ if limited => ("429 Too Many Requests", r#"{"error":"slow down"}"#),
                    "refused" => ("403 Forbidden", r#"{"error":"no"}"#),
                    _ => ("200 OK", r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#),
                };
                log.lock().unwrap().push(key);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        let embedder = HttpEmbedder::new(EmbedConfig {
            base_url,
            keys: vec!["limited".into(), "refused".into(), "good".into()],
            model: "m".into(),
            dims: 2,
            api: Api::OpenAi,
            batch: false,
        });
        let texts = vec!["a".to_string()];
        let timeout = Duration::from_secs(5);
        assert_eq!(embedder.embed(&texts, timeout).unwrap(), [vec![1.0, 0.0]]);
        assert_eq!(*seen.lock().unwrap(), ["limited", "refused", "good"]);
        // The working key stays current for the next request.
        embedder.embed(&texts, timeout).unwrap();
        assert_eq!(seen.lock().unwrap()[3..], ["good"]);

        // With the only usable key rate-limited, the refused one is not sent
        // again and the error stays a retryable 429; the key then recovers.
        let embedder = HttpEmbedder::new(EmbedConfig {
            base_url: embedder.config.base_url.clone(),
            keys: vec!["recovers".into(), "refused".into()],
            ..embedder.config.clone()
        });
        let from = seen.lock().unwrap().len();
        let error = embedder.embed(&texts, timeout).unwrap_err();
        assert_eq!((error.status, error.retryable), (Some(429), true));
        let error = embedder.embed(&texts, timeout).unwrap_err();
        assert_eq!(error.status, Some(429));
        recovered.store(true, std::sync::atomic::Ordering::SeqCst);
        embedder.embed(&texts, timeout).unwrap();
        assert_eq!(
            seen.lock().unwrap()[from..],
            ["recovers", "refused", "recovers", "recovers"]
        );
    }
}
