//! Optional on-device sticker embeddings. Fail-open: a missing model or a
//! failed run leaves [`super::sticker::pick_from_roster`] as the only ranker.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use litert_lm::embedding::{EmbeddingConfig, EmbeddingEngine, EmbeddingPart};
use litert_lm::engine::Backend;
use litert_lm::model_info::{Modality, ModelKind};
use sha2::{Digest, Sha256};

use super::sticker::{RosterItem, pick_from_roster, pick_from_roster_with_vectors};
use crate::db::entity::emoji;
use crate::db::sea::cap::Db;
use crate::db::sea::ops::preference as pref_ops;

pub const EMBEDDER_PREFERENCE: &str = "emoji.embedder";

const QUERY_PREFIX: &str = "task: search query | query: ";
const DOCUMENT_PREFIX: &str = "task: search result | query: ";

struct Loaded {
    path: PathBuf,
    engine: EmbeddingEngine,
    vision: bool,
    cache: HashMap<String, (String, Vec<f32>)>,
}

static LOADED: OnceLock<Mutex<Option<Loaded>>> = OnceLock::new();

fn loaded() -> &'static Mutex<Option<Loaded>> {
    LOADED.get_or_init(|| Mutex::new(None))
}

pub fn document_text(name: &str, tags: Option<&str>) -> String {
    match tags.map(str::trim).filter(|s| !s.is_empty()) {
        Some(tags) => format!("{DOCUMENT_PREFIX}{name} {tags}"),
        None => format!("{DOCUMENT_PREFIX}{name}"),
    }
}

pub fn query_text(query: &str) -> String {
    format!("{QUERY_PREFIX}{query}")
}

fn source_hash(name: &str, tags: Option<&str>, image: Option<&[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0x1f]);
    hasher.update(tags.unwrap_or("").as_bytes());
    hasher.update([0x1f]);
    if let Some(image) = image {
        hasher.update(image);
    }
    format!("{:x}", hasher.finalize())
}

fn vision_backend(info: &litert_lm::model_info::ModelInfo) -> Option<Backend> {
    let backends = info.backends(Modality::Vision);
    if backends.contains(&Backend::Cpu) {
        Some(Backend::Cpu)
    } else {
        backends.first().copied()
    }
}

fn open_engine(path: &Path) -> Result<(EmbeddingEngine, bool), String> {
    let rt = crate::provider::litert_lm_provider::runtime_for_embedder()?;
    let path_str = path.to_str().ok_or("embedder path is not UTF-8")?;
    let info = rt.inspect_model(path_str).map_err(|e| e.to_string())?;
    if info.kind != ModelKind::Embedding {
        return Err("emoji.embedder must be an embedding .litertlm, not a chat model".into());
    }
    let mut config = EmbeddingConfig::new(path_str, Backend::Cpu);
    let vision = vision_backend(&info).or(Some(Backend::Cpu));
    if let Some(backend) = vision {
        config.vision_backend = Some(backend);
        match EmbeddingEngine::new(&rt, &config) {
            Ok(engine) => return Ok((engine, true)),
            Err(error) => {
                tracing::warn!(error = %error, "embedder vision backend refused; using text only");
                config.vision_backend = None;
            }
        }
    }
    Ok((EmbeddingEngine::new(&rt, &config).map_err(|e| e.to_string())?, false))
}

/// Best effort: a missing or failed embedder uses the lexical pick unchanged.
pub async fn try_pick<'a, T: RosterItem>(
    roster: &'a [T],
    sticker_id: Option<&str>,
    query: Option<&str>,
    db: &Db,
) -> Result<&'a T, String> {
    try_pick_with_images(roster, sticker_id, query, db, |_, _| None).await
}

pub async fn try_pick_emojis<'a>(
    roster: &'a [emoji::Model],
    sticker_id: Option<&str>,
    query: Option<&str>,
    db: &Db,
    data_dir: Option<&Path>,
) -> Result<&'a emoji::Model, String> {
    try_pick_with_images(roster, sticker_id, query, db, |item, vision| {
        if !vision {
            return None;
        }
        let data_dir = data_dir?;
        if item.file_name.is_empty() {
            return None;
        }
        let path = crate::emoji::emoji_path(data_dir, &item.pack_id, &item.file_name);
        match crate::emoji::embedding_image_bytes(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                tracing::warn!(error = %error, sticker_id = %item.id, "sticker image skipped for embedding");
                None
            }
        }
    })
    .await
}

async fn try_pick_with_images<'a, T: RosterItem>(
    roster: &'a [T],
    sticker_id: Option<&str>,
    query: Option<&str>,
    db: &Db,
    image_of: impl Fn(&T, bool) -> Option<Vec<u8>>,
) -> Result<&'a T, String> {
    let path = pref_ops::get_preference(db, EMBEDDER_PREFERENCE)
        .await
        .map_err(|e| e.to_string())?;
    let Some(path) = path.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) else {
        *loaded().lock().unwrap_or_else(|e| e.into_inner()) = None;
        return pick_from_roster(roster, sticker_id, query);
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        tracing::warn!("emoji.embedder is not an absolute path; using lexical sticker pick");
        return pick_from_roster(roster, sticker_id, query);
    }

    match pick_with_engine(roster, sticker_id, query, &path, image_of) {
        Ok(item) => Ok(item),
        Err(error) => {
            tracing::warn!(error = %error, "sticker embedder unavailable; using lexical pick");
            pick_from_roster(roster, sticker_id, query)
        }
    }
}

fn pick_with_engine<'a, T: RosterItem>(
    roster: &'a [T],
    sticker_id: Option<&str>,
    query: Option<&str>,
    path: &Path,
    image_of: impl Fn(&T, bool) -> Option<Vec<u8>>,
) -> Result<&'a T, String> {
    let mut slot = loaded().lock().unwrap_or_else(|e| e.into_inner());
    let need_reload = match slot.as_ref() {
        Some(loaded) => loaded.path != path,
        None => true,
    };
    if need_reload {
        let (engine, vision) = open_engine(path)?;
        *slot = Some(Loaded {
            path: path.to_path_buf(),
            engine,
            vision,
            cache: HashMap::new(),
        });
    }
    let loaded = slot.as_mut().expect("reload leaves Some");

    let query_trimmed = query.map(str::trim).filter(|s| !s.is_empty());
    let query_vec = match query_trimmed {
        Some(q) => Some(loaded.engine.embed_text(&query_text(q)).map_err(|e| e.to_string())?),
        None => None,
    };

    let mut item_vecs: HashMap<String, Vec<f32>> = HashMap::new();
    if query_vec.is_some() {
        for item in roster {
            let image = image_of(item, loaded.vision);
            let hash = source_hash(item.roster_name(), item.roster_tags(), image.as_deref());
            if let Some((cached_hash, vec)) = loaded.cache.get(item.roster_id())
                && cached_hash == &hash
            {
                item_vecs.insert(item.roster_id().to_string(), vec.clone());
                continue;
            }
            let doc = document_text(item.roster_name(), item.roster_tags());
            let vec = match image.as_deref() {
                Some(bytes) => match loaded
                    .engine
                    .embed_parts(&[EmbeddingPart::Image(bytes), EmbeddingPart::Text(&doc)])
                {
                    Ok(vec) => vec,
                    Err(error) => {
                        tracing::warn!(error = %error, "sticker image embed failed; using name and tags");
                        loaded.engine.embed_text(&doc).map_err(|e| e.to_string())?
                    }
                },
                None => loaded.engine.embed_text(&doc).map_err(|e| e.to_string())?,
            };
            loaded.cache.insert(item.roster_id().to_string(), (hash, vec.clone()));
            item_vecs.insert(item.roster_id().to_string(), vec);
        }
    }
    drop(slot);

    pick_from_roster_with_vectors(roster, sticker_id, query, query_vec.as_deref(), |item| {
        item_vecs.get(item.roster_id()).map(Vec::as_slice)
    })
}

#[cfg(test)]
mod tests {
    use super::{document_text, query_text, source_hash};

    #[test]
    fn prefixes_are_the_embeddinggemma_search_pair() {
        assert_eq!(query_text("无语"), "task: search query | query: 无语");
        assert_eq!(
            document_text("黑人问号", Some("尴尬,无语")),
            "task: search result | query: 黑人问号 尴尬,无语"
        );
        assert_eq!(document_text("点赞", None), "task: search result | query: 点赞");
    }

    #[test]
    fn source_hash_changes_when_the_image_bytes_change() {
        let name = "点赞";
        let without = source_hash(name, None, None);
        let with = source_hash(name, None, Some(b"png-a"));
        let other = source_hash(name, None, Some(b"png-b"));
        assert_ne!(without, with);
        assert_ne!(with, other);
        assert_eq!(with, source_hash(name, None, Some(b"png-a")));
    }
}
