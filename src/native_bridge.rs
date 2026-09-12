//! C ABI used by the Android host. Handles are registry IDs, never Rust pointers.
use crate::protocol::{Event, Op};
use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char},
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

struct NativeCore {
    ops: mpsc::Sender<Op>,
    events: Mutex<std::sync::mpsc::Receiver<String>>,
    wake: std::sync::mpsc::SyncSender<String>,
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
}
static HANDLES: OnceLock<Mutex<HashMap<u64, Arc<NativeCore>>>> = OnceLock::new();
static NEXT: AtomicU64 = AtomicU64::new(1);
fn handles() -> &'static Mutex<HashMap<u64, Arc<NativeCore>>> {
    HANDLES.get_or_init(Default::default)
}
fn lookup(id: u64) -> Option<Arc<NativeCore>> {
    handles().lock().ok()?.get(&id).cloned()
}

/// `data_dir` must be a valid NUL-terminated UTF-8 path owned by the application.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gnomeai_create(data_dir: *const c_char) -> u64 {
    std::panic::catch_unwind(|| {
        if data_dir.is_null() { return 0; }
        let Ok(path) = (unsafe { CStr::from_ptr(data_dir) }).to_str() else { return 0; };
        let home = PathBuf::from(path);
        if !home.is_absolute() { return 0; }
        let Ok(runtime) = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() else { return 0; };
        let (ops, op_rx) = mpsc::channel(256);
        let (event_tx, mut event_rx) = mpsc::channel(1024);
        let (out, events) = std::sync::mpsc::sync_channel(2048);
        let wake = out.clone();
        let providers: Vec<_> = crate::provider_catalog::PROVIDERS.iter()
            .filter(|p| !matches!(p.auth, crate::provider_catalog::AuthKind::Account))
            .map(|p| serde_json::json!({"id":p.id,"name":p.name,"base_url":p.base_url,
                "default_model":p.default_model,"auth":"api_key","description":p.description})).collect();
        let _ = out.send(serde_json::json!({"event":"ui_config", "version":env!("CARGO_PKG_VERSION"), "providers":providers}).to_string());
        // Forwarding onto a bounded synchronous queue runs on a blocking worker;
        // a paused Android activity never blocks the async executor threads.
        runtime.spawn_blocking(move || {
            while let Some(event) = event_rx.blocking_recv() {
                if let Ok(json) = serde_json::to_string(&event) {
                    if out.send(json).is_err() { break; }
                }
            }
        });
        runtime.spawn(async move {
            if let Err(error) = crate::agent_runtime::run_native(home, op_rx, event_tx.clone()).await {
                let _ = event_tx.send(Event::Error { message:error.to_string(), fatal:true }).await;
            }
        });
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let core = Arc::new(NativeCore { ops, wake, events:Mutex::new(events), runtime:Mutex::new(Some(runtime)) });
        if let Ok(mut map) = handles().lock() { map.insert(id, core); id } else { 0 }
    }).unwrap_or(0)
}

/// Returns 0 on success, -1 for invalid input/handle, -2 for backpressure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gnomeai_send_json(id: u64, json: *const c_char) -> i32 {
    std::panic::catch_unwind(|| {
        let Some(core) = lookup(id) else {
            return -1;
        };
        if json.is_null() {
            return -1;
        }
        let bytes = unsafe { CStr::from_ptr(json) }.to_bytes();
        if bytes.len() > 32 * 1024 * 1024 {
            return -1;
        }
        let Ok(op) = serde_json::from_slice(bytes) else {
            return -1;
        };
        match core.ops.try_send(op) {
            Ok(()) => 0,
            Err(mpsc::error::TrySendError::Full(_)) => -2,
            Err(_) => -1,
        }
    })
    .unwrap_or(-1)
}

/// Block until an event or explicit reader interrupt. Free non-null results exactly once.
#[unsafe(no_mangle)]
pub extern "C" fn gnomeai_next_event(id: u64) -> *mut c_char {
    std::panic::catch_unwind(|| {
        let core = lookup(id)?;
        let text = core
            .events
            .lock()
            .ok()?
            .recv()
            .ok()?;
        CString::new(text).ok().map(CString::into_raw)
    })
    .ok()
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

/// Wakes a stopped managed reader without waiting for a polling timeout.
#[unsafe(no_mangle)]
pub extern "C" fn gnomeai_interrupt_events(id: u64) {
    let _ = std::panic::catch_unwind(|| {
        if let Some(core) = lookup(id) { let _ = core.wake.try_send("{}".to_owned()); }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gnomeai_free_string(text: *mut c_char) {
    if !text.is_null() {
        drop(unsafe { CString::from_raw(text) });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn gnomeai_destroy(id: u64) {
    let _ = std::panic::catch_unwind(|| {
        let core = handles().lock().ok().and_then(|mut map| map.remove(&id));
        if let Some(core) = core {
            let _ = core.ops.try_send(Op::Shutdown);
            // Caller stops the event reader before destroy; dropping the receiver
            // releases the bounded forwarding worker before shutting down Tokio.
            if let Ok(core) = Arc::try_unwrap(core) {
                drop(core.events);
                if let Ok(Some(runtime)) = core.runtime.into_inner() {
                    runtime.shutdown_timeout(Duration::from_secs(2));
                }
            }
        }
    });
}
