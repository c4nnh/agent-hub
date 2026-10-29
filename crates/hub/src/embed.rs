use std::sync::Mutex;

pub const DIMS: usize = 384;

pub trait Embedder: Send + Sync {
    /// Returns a DIMS-length vector for the text.
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// Deterministic test embedder: each byte trigram is hashed into a bucket, then L2-normalized.
pub struct HashEmbedder;

impl Embedder for HashEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        let bytes = text.as_bytes();
        let mut v = vec![0f32; DIMS];
        let mut add = |window: &[u8]| {
            // FNV-1a
            let mut h: u64 = 0xcbf29ce484222325;
            for b in window {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            v[(h % DIMS as u64) as usize] += 1.0;
        };
        if bytes.len() < 3 {
            add(bytes);
        }
        for w in bytes.windows(3) {
            add(w);
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm == 0.0 {
            v[0] = 1.0;
            return v;
        }
        v.iter_mut().for_each(|x| *x /= norm);
        v
    }
}

/// bge-small-en-v1.5 via fastembed. Downloads the model into ~/.cache/ah-hub on first use.
pub struct FastEmbedder(Mutex<fastembed::TextEmbedding>);

impl FastEmbedder {
    pub fn new() -> Result<Self, String> {
        let home = std::env::var("HOME").map_err(|e| e.to_string())?;
        let cache = std::path::PathBuf::from(home).join(".cache/ah-hub");
        let opts = fastembed::TextInitOptions::new(fastembed::EmbeddingModel::BGESmallENV15)
            .with_cache_dir(cache);
        let model = fastembed::TextEmbedding::try_new(opts).map_err(|e| e.to_string())?;
        Ok(Self(Mutex::new(model)))
    }
}

impl Embedder for FastEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut model = self.0.lock().expect("embedder lock");
        let mut out = model.embed([text], None).expect("embed text");
        out.remove(0)
    }
}
