//! JNI bridge to the Android side (FileBridge.kt / MainActivity.kt).
//! Only compiled on Android. SAF file operations and permission queries live here.
//!
//! ndk-context is initialized by `initNdkContext` (called from MainActivity.onCreate
//! via lib.rs `Java_..._initNdkContext`), NOT by Tauri/tao itself.
#![cfg(target_os = "android")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};

use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString, JValue};
use tokio::sync::oneshot;

const BRIDGE_CLASS: &str = "cn.yuxiaoqiu.meridian.FileBridge";

use jni::objects::GlobalRef;

static BRIDGE_REF: OnceLock<GlobalRef> = OnceLock::new();

/// Get the JavaVM via ndk-context.
fn java_vm() -> Result<jni::JavaVM, String> {
    let ctx = ndk_context::android_context();
    unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.map_err(|e| format!("failed to get JavaVM: {e}"))
}

/// Attach to the JVM and run `f` with the JNI env and the application context.
/// Pending Java exceptions are converted into Err strings.
/// Uses `attach_current_thread_permanently` to avoid the GlobalRef-drop-on-
/// detached-thread warning that `AttachGuard` causes with jni 0.21.
/// All local references created inside `f` (and by `describe_exception`) are
/// released when the local frame pops, preventing table overflow on the
/// permanently-attached tokio blocking threads.
fn with_env<T>(f: impl FnOnce(&mut JNIEnv, &JObject) -> Result<T, jni::errors::Error>) -> Result<T, String> {
    let vm = java_vm()?;
    let mut env = vm
        .attach_current_thread_permanently()
        .map_err(|e| format!("failed to attach JNI thread: {e}"))?;
    let ctx = ndk_context::android_context();
    // context is a global ref from ndk-context, not a frame-local ref — safe to
    // borrow across the local frame boundary.
    let context = unsafe { JObject::from_raw(ctx.context().cast()) };
    let outcome: Result<Result<T, String>, jni::errors::Error> = env.with_local_frame(32, |env| {
        Ok(match f(env, &context) {
            Ok(v) => Ok(v),
            Err(jni::errors::Error::JavaException) => Err(describe_exception(env)),
            Err(e) => Err(format!("JNI error: {e}")),
        })
    });
    outcome.unwrap_or_else(|e| Err(format!("JNI local frame error: {e}")))
}

fn describe_exception(env: &mut JNIEnv) -> String {
    let Ok(throwable) = env.exception_occurred() else {
        return "Java exception (no details)".to_string();
    };
    let _ = env.exception_clear();
    if let Ok(msg) = env.call_method(&throwable, "toString", "()Ljava/lang/String;", &[]) {
        if let Ok(obj) = msg.l() {
            if let Ok(s) = env.get_string(&JString::from(obj)) {
                let s: String = s.into();
                if s.contains("SecurityException") {
                    return format!(
                        "Directory authorization has been revoked or expired. \
                         Ask the user to re-authorize this folder in Settings > File Access. \
                         ({})",
                        s
                    );
                }
                return s;
            }
        }
    }
    "Java exception (no details)".to_string()
}

/// Load the FileBridge class once and cache it as a GlobalRef.
/// Subsequent calls return the cached reference.
/// Uses double-checked init instead of `get_or_init` so errors propagate
/// instead of panicking (a race loser's GlobalRef is harmlessly dropped).
fn bridge_class<'l>(env: &mut JNIEnv<'l>, context: &JObject) -> Result<JClass<'l>, jni::errors::Error> {
    if BRIDGE_REF.get().is_none() {
        let loader = env
            .call_method(context, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
            .l()?;
        let class_name = env.new_string(BRIDGE_CLASS)?;
        let cls = env
            .call_method(
                &loader,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[JValue::Object(&class_name)],
            )?
            .l()?;
        let global = env.new_global_ref(&cls)?;
        let _ = BRIDGE_REF.set(global);
    }
    let global = BRIDGE_REF.get().expect("just initialized above");
    let local = env.new_local_ref(global.as_obj())?;
    Ok(JClass::from(local))
}

pub fn is_manage_storage_granted() -> Result<bool, String> {
    with_env(|env, context| {
        let cls = bridge_class(env, context)?;
        env.call_static_method(
            &cls,
            "isManageStorageGranted",
            "(Landroid/content/Context;)Z",
            &[JValue::Object(context)],
        )?
        .z()
    })
}

pub fn open_manage_storage_settings() -> Result<(), String> {
    with_env(|env, context| {
        let cls = bridge_class(env, context)?;
        env.call_static_method(
            &cls,
            "openManageStorageSettings",
            "(Landroid/content/Context;)V",
            &[JValue::Object(context)],
        )?;
        Ok(())
    })
}

// ---- Media picker (camera / gallery) ----

pub type MediaPickResult = Option<String>; // content:// URI, None = cancelled

static MEDIA_WAITERS: OnceLock<Mutex<HashMap<i32, oneshot::Sender<MediaPickResult>>>> = OnceLock::new();
static NEXT_MEDIA_REQ: AtomicI32 = AtomicI32::new(1);

pub fn media_waiters() -> &'static Mutex<HashMap<i32, oneshot::Sender<MediaPickResult>>> {
    MEDIA_WAITERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn take_photo() -> Result<MediaPickResult, String> {
    let req_id = NEXT_MEDIA_REQ.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    media_waiters().lock().unwrap().insert(req_id, tx);

    let launched = with_env(|env, context| {
        let cls = bridge_class(env, context)?;
        env.call_static_method(&cls, "launchCamera", "(I)Z", &[JValue::Int(req_id)])?
            .z()
    });
    match launched {
        Ok(true) => {}
        Ok(false) => {
            media_waiters().lock().unwrap().remove(&req_id);
            return Err("no active window to launch camera".to_string());
        }
        Err(e) => {
            media_waiters().lock().unwrap().remove(&req_id);
            return Err(e);
        }
    }

    match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(_)) => Err("camera closed unexpectedly".to_string()),
        Err(_) => {
            media_waiters().lock().unwrap().remove(&req_id);
            Err("camera timed out".to_string())
        }
    }
}

pub async fn pick_gallery() -> Result<MediaPickResult, String> {
    let req_id = NEXT_MEDIA_REQ.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    media_waiters().lock().unwrap().insert(req_id, tx);

    let launched = with_env(|env, context| {
        let cls = bridge_class(env, context)?;
        env.call_static_method(&cls, "launchGallery", "(I)Z", &[JValue::Int(req_id)])?
            .z()
    });
    match launched {
        Ok(true) => {}
        Ok(false) => {
            media_waiters().lock().unwrap().remove(&req_id);
            return Err("no active window to launch gallery".to_string());
        }
        Err(e) => {
            media_waiters().lock().unwrap().remove(&req_id);
            return Err(e);
        }
    }

    match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(_)) => Err("gallery picker closed unexpectedly".to_string()),
        Err(_) => {
            media_waiters().lock().unwrap().remove(&req_id);
            Err("gallery picker timed out".to_string())
        }
    }
}

pub fn clean_camera_cache() {
    let _ = with_env(|env, context| {
        let cls = bridge_class(env, context)?;
        env.call_static_method(
            &cls,
            "cleanCameraCache",
            "(Landroid/content/Context;)V",
            &[JValue::Object(context)],
        )?;
        Ok(())
    });
}

// ---- SAF directory picker ----

pub type SafPickResult = Option<(String, String)>; // (tree_uri, display_name), None = cancelled

static SAF_WAITERS: OnceLock<Mutex<HashMap<i32, oneshot::Sender<SafPickResult>>>> = OnceLock::new();
static NEXT_SAF_REQ: AtomicI32 = AtomicI32::new(1);

pub fn saf_waiters() -> &'static Mutex<HashMap<i32, oneshot::Sender<SafPickResult>>> {
    SAF_WAITERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Launch the system directory picker. Resolves to None if the user cancels.
pub async fn pick_directory() -> Result<SafPickResult, String> {
    {
        let waiters = saf_waiters().lock().unwrap();
        if !waiters.is_empty() {
            return Err("a directory picker is already open".to_string());
        }
    }

    let req_id = NEXT_SAF_REQ.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    saf_waiters().lock().unwrap().insert(req_id, tx);

    let launched = with_env(|env, context| {
        let cls = bridge_class(env, context)?;
        env.call_static_method(&cls, "launchDirectoryPicker", "(I)Z", &[JValue::Int(req_id)])?
            .z()
    });
    match launched {
        Ok(true) => {}
        Ok(false) => {
            saf_waiters().lock().unwrap().remove(&req_id);
            return Err("no active window to show the directory picker".to_string());
        }
        Err(e) => {
            saf_waiters().lock().unwrap().remove(&req_id);
            return Err(e);
        }
    }

    match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(_)) => Err("directory picker closed unexpectedly".to_string()),
        Err(_) => {
            saf_waiters().lock().unwrap().remove(&req_id);
            Err("directory picker timed out".to_string())
        }
    }
}

/// Release a persisted SAF grant when the user removes an authorized directory.
pub fn release_persisted_uri(tree_uri: &str) -> Result<(), String> {
    with_env(|env, context| {
        let resolver = env
            .call_method(
                context,
                "getContentResolver",
                "()Landroid/content/ContentResolver;",
                &[],
            )?
            .l()?;
        let uri_str = env.new_string(tree_uri)?;
        let uri = env
            .call_static_method(
                "android/net/Uri",
                "parse",
                "(Ljava/lang/String;)Landroid/net/Uri;",
                &[JValue::Object(&uri_str)],
            )?
            .l()?;
        // FLAG_GRANT_READ_URI_PERMISSION | FLAG_GRANT_WRITE_URI_PERMISSION = 0x1 | 0x2
        env.call_method(
            &resolver,
            "releasePersistableUriPermission",
            "(Landroid/net/Uri;I)V",
            &[JValue::Object(&uri), JValue::Int(3)],
        )?;
        Ok(())
    })
}

pub fn persisted_tree_uris() -> Result<Vec<String>, String> {
    let json: String = with_env(|env, context| {
        let cls = bridge_class(env, context)?;
        let s = env
            .call_static_method(
                &cls,
                "persistedTreeUris",
                "(Landroid/content/Context;)Ljava/lang/String;",
                &[JValue::Object(context)],
            )?
            .l()?;
        env.get_string(&JString::from(s)).map(Into::into)
    })?;
    serde_json::from_str(&json).map_err(|e| format!("invalid uri list: {e}"))
}

// ---- SAF file operations (blocking JNI wrapped in spawn_blocking) ----

fn call_saf_string(method: &'static str, sig: &'static str, args: Vec<String>) -> Result<String, String> {
    with_env(|env, context| {
        let cls = bridge_class(env, context)?;
        let jstrings: Vec<JString> = args.iter().map(|a| env.new_string(a)).collect::<Result<_, _>>()?;
        let mut jargs: Vec<JValue> = vec![JValue::Object(context)];
        jargs.extend(jstrings.iter().map(|s| JValue::Object(s)));
        let result = env.call_static_method(&cls, method, sig, &jargs)?;
        let obj = JString::from(result.l()?);
        let s = env.get_string(&obj)?;
        Ok(s.into())
    })
}

#[derive(serde::Deserialize)]
pub struct SafRead {
    pub content: String,
    pub truncated: bool,
    pub size: Option<u64>,
}

pub async fn saf_read(tree_uri: &str, rel: &str, max_bytes: i64) -> Result<SafRead, String> {
    let (tree, rel) = (tree_uri.to_string(), rel.to_string());
    let json = tokio::task::spawn_blocking(move || {
        with_env(|env, context| {
            let cls = bridge_class(env, context)?;
            let jtree = env.new_string(&tree)?;
            let jrel = env.new_string(&rel)?;
            let result = env
                .call_static_method(
                    &cls,
                    "safRead",
                    "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;J)Ljava/lang/String;",
                    &[
                        JValue::Object(context),
                        JValue::Object(&jtree),
                        JValue::Object(&jrel),
                        JValue::Long(max_bytes),
                    ],
                )?
                .l()?;
            let jstr = JString::from(result);
            let s = env.get_string(&jstr)?;
            Ok(String::from(s))
        })
    })
    .await
    .map_err(|e| format!("task failed: {e}"))??;
    serde_json::from_str(&json).map_err(|e| format!("invalid safRead response: {e}"))
}

pub async fn saf_write(tree_uri: &str, rel: &str, content: &str) -> Result<(), String> {
    let (tree, rel, content) = (tree_uri.to_string(), rel.to_string(), content.to_string());
    tokio::task::spawn_blocking(move || {
        with_env(|env, context| {
            let cls = bridge_class(env, context)?;
            let tree = env.new_string(&tree)?;
            let rel = env.new_string(&rel)?;
            let content = env.new_string(&content)?;
            env.call_static_method(
                &cls,
                "safWrite",
                "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
                &[
                    JValue::Object(context),
                    JValue::Object(&tree),
                    JValue::Object(&rel),
                    JValue::Object(&content),
                ],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| format!("task failed: {e}"))?
}

pub async fn saf_list(tree_uri: &str, rel: &str) -> Result<Vec<crate::tools::backend::DirEntry>, String> {
    let (tree, rel) = (tree_uri.to_string(), rel.to_string());
    let json = tokio::task::spawn_blocking(move || {
        call_saf_string(
            "safList",
            "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            vec![tree, rel],
        )
    })
    .await
    .map_err(|e| format!("task failed: {e}"))??;

    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        is_dir: bool,
        size: Option<u64>,
    }
    let entries: Vec<Entry> = serde_json::from_str(&json).map_err(|e| format!("invalid listing from bridge: {e}"))?;
    Ok(entries
        .into_iter()
        .map(|e| crate::tools::backend::DirEntry {
            name: e.name,
            is_dir: e.is_dir,
            is_symlink: false,
            size: e.size,
        })
        .collect())
}

pub async fn saf_delete(tree_uri: &str, rel: &str, recursive: bool) -> Result<(), String> {
    let (tree, rel) = (tree_uri.to_string(), rel.to_string());
    tokio::task::spawn_blocking(move || {
        with_env(|env, context| {
            let cls = bridge_class(env, context)?;
            let tree = env.new_string(&tree)?;
            let rel = env.new_string(&rel)?;
            env.call_static_method(
                &cls,
                "safDelete",
                "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;Z)V",
                &[
                    JValue::Object(context),
                    JValue::Object(&tree),
                    JValue::Object(&rel),
                    JValue::Bool(recursive as u8),
                ],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| format!("task failed: {e}"))?
}

pub async fn saf_rename(tree_uri: &str, from_rel: &str, to_rel: &str) -> Result<(), String> {
    let (tree, from_rel, to_rel) = (tree_uri.to_string(), from_rel.to_string(), to_rel.to_string());
    tokio::task::spawn_blocking(move || {
        with_env(|env, context| {
            let cls = bridge_class(env, context)?;
            let tree = env.new_string(&tree)?;
            let from = env.new_string(&from_rel)?;
            let to = env.new_string(&to_rel)?;
            env.call_static_method(
                &cls,
                "safRename",
                "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
                &[
                    JValue::Object(context),
                    JValue::Object(&tree),
                    JValue::Object(&from),
                    JValue::Object(&to),
                ],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| format!("task failed: {e}"))?
}

// ---- Content URI helpers (for attachment upload) ----

pub async fn content_stat(uri: &str) -> Result<ContentStat, String> {
    let uri = uri.to_string();
    let json = tokio::task::spawn_blocking(move || {
        call_saf_string(
            "contentStat",
            "(Landroid/content/Context;Ljava/lang/String;)Ljava/lang/String;",
            vec![uri],
        )
    })
    .await
    .map_err(|e| format!("task failed: {e}"))??;
    serde_json::from_str(&json).map_err(|e| format!("invalid content stat: {e}"))
}

#[derive(serde::Deserialize)]
pub struct ContentStat {
    pub name: Option<String>,
    pub size: Option<u64>,
    pub mime: Option<String>,
}

pub async fn content_copy(uri: &str, dest_abs: &str) -> Result<(), String> {
    let (uri, dest) = (uri.to_string(), dest_abs.to_string());
    tokio::task::spawn_blocking(move || {
        with_env(|env, context| {
            let cls = bridge_class(env, context)?;
            let juri = env.new_string(&uri)?;
            let jdest = env.new_string(&dest)?;
            env.call_static_method(
                &cls,
                "contentCopy",
                "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;)V",
                &[JValue::Object(context), JValue::Object(&juri), JValue::Object(&jdest)],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|e| format!("task failed: {e}"))?
}
