//! Java ↔ Rust bridge on `dev.uplink.UplinkActivity`.
//!
//! [`Platform::attach`] registers the natives and hands the activity a `jlong` handle to an
//! `Arc<Inner>`; Java passes it back on each callback and releases it in `onDestroy`. No globals:
//! `Platform` owns its `JavaVM` and a global ref to the activity. Android constants are read from
//! their Java classes.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use android_activity::AndroidApp;
use jni::objects::{JByteArray, JClass, JIntArray, JObject, JObjectArray, JString, JValue};
use jni::refs::Global;
use jni::strings::JNIStr;
use jni::sys::{jint, jlong};
use jni::{Env, EnvUnowned, JavaVM, NativeMethod, Outcome, jni_sig, jni_str};
use tokio::sync::oneshot;

use crate::Error;

/// `getHistoricalProcessExitReasons` pid filter: all processes of the package.
const ALL_PIDS: i32 = 0;
const EXIT_RECORDS: i32 = 3;
const TRACE_MIN_RUN: usize = 6;
const TRACE_MAX_LINES: usize = 30;
const SINGLE_PERMISSION: i32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Camera,
    RecordAudio,
}

impl Permission {
    /// Field of `android.Manifest.permission` holding the permission string.
    const fn manifest_field(self) -> &'static JNIStr {
        match self {
            Self::Camera => jni_str!("CAMERA"),
            Self::RecordAudio => jni_str!("RECORD_AUDIO"),
        }
    }
}

/// State shared with Java through the activity's handle.
struct Inner {
    permission_granted: jint,
    next_request_code: AtomicI32,
    pending: Mutex<HashMap<jint, oneshot::Sender<bool>>>,
}

impl Inner {
    fn complete(&self, request_code: jint, granted: bool) {
        let sender = self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&request_code);
        match sender {
            // The receiver may have been dropped by a caller that stopped waiting.
            Some(tx) => drop(tx.send(granted)),
            None => tracing::warn!(request_code, "permission result for unknown request"),
        }
    }
}

pub struct Platform {
    vm: JavaVM,
    activity: Global<JObject<'static>>,
    inner: Arc<Inner>,
}

impl Platform {
    pub fn attach(app: &AndroidApp) -> Result<Self, Error> {
        // SAFETY: android-activity guarantees a valid JavaVM pointer for the process lifetime.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let (activity, inner) = vm.attach_current_thread(|env| -> Result<_, Error> {
            // SAFETY: a global ref to the activity owned by android-activity; not deleted here.
            let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
            let class = env.get_object_class(&activity)?;
            // SAFETY: the function pointers match the Java declarations in UplinkActivity.
            unsafe { env.register_native_methods(&class, &natives())? };
            let inner = Arc::new(Inner {
                permission_granted: env
                    .get_static_field(jni_str!("android/content/pm/PackageManager"), jni_str!("PERMISSION_GRANTED"), jni_sig!("I"))?
                    .i()?,
                next_request_code: AtomicI32::new(0),
                pending: Mutex::default(),
            });
            let handle = Arc::into_raw(Arc::clone(&inner)).expose_provenance();
            let attached = handle_from_address(handle).and_then(|handle| {
                env.call_method(&activity, jni_str!("attachNative"), jni_sig!("(J)V"), &[JValue::Long(handle)])?;
                Ok(())
            });
            if let Err(e) = attached {
                // SAFETY: reclaims the reference leaked above; Java never saw it.
                drop(unsafe { Arc::from_raw(std::ptr::with_exposed_provenance::<Inner>(handle)) });
                return Err(e);
            }
            Ok((env.new_global_ref(&activity)?, inner))
        })?;
        tracing::debug!("platform bridge attached");
        Ok(Self { vm, activity, inner })
    }

    fn with_activity<T>(&self, f: impl FnOnce(&mut Env, &JObject) -> Result<T, Error>) -> Result<T, Error> {
        self.vm.attach_current_thread(|env| f(env, self.activity.as_obj()))
    }

    pub fn has_permission(&self, permission: Permission) -> Result<bool, Error> {
        let granted = self.inner.permission_granted;
        self.with_activity(|env, activity| {
            let name = permission_string(env, permission)?;
            let state = env
                .call_method(activity, jni_str!("checkSelfPermission"), jni_sig!("(Ljava/lang/String;)I"), &[JValue::Object(&name)])?
                .i()?;
            Ok(state == granted)
        })
    }

    /// Resolves to whether the permission is granted, prompting the user if needed.
    pub async fn request_permission(&self, permission: Permission) -> Result<bool, Error> {
        if self.has_permission(permission)? {
            return Ok(true);
        }
        let code = self.inner.next_request_code.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(code, tx);
        let requested = self.with_activity(|env, activity| {
            let name = permission_string(env, permission)?;
            let names = env.new_object_array(SINGLE_PERMISSION, jni_str!("java/lang/String"), &name)?;
            env.call_method(
                activity,
                jni_str!("requestPermissionsAsync"),
                jni_sig!("([Ljava/lang/String;I)V"),
                &[JValue::Object(&names), JValue::Int(code)],
            )?;
            Ok(())
        });
        if let Err(e) = requested {
            self.inner.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&code);
            return Err(e);
        }
        tracing::debug!(?permission, code, "permission requested");
        rx.await.map_err(|_| Error::RequestAbandoned)
    }

    /// Recent `ApplicationExitInfo` records, with printable excerpts of crash/ANR traces
    /// (native crashes carry a tombstone protobuf).
    pub fn previous_exits(&self) -> Result<String, Error> {
        self.with_activity(|env, activity| {
            let traced = [
                exit_reason(env, jni_str!("REASON_CRASH"))?,
                exit_reason(env, jni_str!("REASON_CRASH_NATIVE"))?,
                exit_reason(env, jni_str!("REASON_ANR"))?,
            ];
            let service = env
                .get_static_field(jni_str!("android/content/Context"), jni_str!("ACTIVITY_SERVICE"), jni_sig!("Ljava/lang/String;"))?
                .l()?;
            let am = env
                .call_method(
                    activity,
                    jni_str!("getSystemService"),
                    jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
                    &[JValue::Object(&service)],
                )?
                .l()?;
            let package = env.call_method(activity, jni_str!("getPackageName"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
            let list = env
                .call_method(
                    &am,
                    jni_str!("getHistoricalProcessExitReasons"),
                    jni_sig!("(Ljava/lang/String;II)Ljava/util/List;"),
                    &[JValue::Object(&package), JValue::Int(ALL_PIDS), JValue::Int(EXIT_RECORDS)],
                )?
                .l()?;
            let count = env.call_method(&list, jni_str!("size"), jni_sig!("()I"), &[])?.i()?;
            let mut out = String::new();
            for i in 0..count {
                let info = env.call_method(&list, jni_str!("get"), jni_sig!("(I)Ljava/lang/Object;"), &[JValue::Int(i)])?.l()?;
                let text = env.call_method(&info, jni_str!("toString"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
                out += &java_string(env, text)?;
                out.push('\n');
                let reason = env.call_method(&info, jni_str!("getReason"), jni_sig!("()I"), &[])?.i()?;
                if traced.contains(&reason) {
                    out += &trace_excerpt(env, &info)?;
                }
            }
            Ok(out)
        })
    }
}

fn natives() -> [NativeMethod<'static>; 2] {
    // SAFETY: signatures match the `extern "system"` functions below and UplinkActivity's natives.
    unsafe {
        [
            NativeMethod::from_raw_parts(
                jni_str!("nativePermissionsResult"),
                jni_str!("(JI[Ljava/lang/String;[I)V"),
                native_permissions_result as *mut c_void,
            ),
            NativeMethod::from_raw_parts(jni_str!("nativeDetach"), jni_str!("(J)V"), native_detach as *mut c_void),
        ]
    }
}

/// Bit-preserving pointer → `jlong`: arm64 Android tags heap pointers in the top byte, so
/// addresses routinely exceed `i64::MAX` as unsigned values.
fn handle_from_address(address: usize) -> Result<jlong, Error> {
    jlong::try_from(address.cast_signed()).map_err(|_| Error::Handle)
}

fn address_from_handle(handle: jlong) -> Option<usize> {
    isize::try_from(handle).ok().map(isize::cast_unsigned)
}

/// # Safety
/// `handle` must come from [`Platform::attach`] and not have been detached.
unsafe fn inner_from<'a>(handle: jlong) -> Option<&'a Inner> {
    let address = address_from_handle(handle)?;
    // SAFETY: per the caller contract the pointer is a live `Arc<Inner>` allocation.
    unsafe { std::ptr::with_exposed_provenance::<Inner>(address).as_ref() }
}

extern "system" fn native_permissions_result<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    request_code: jint,
    _permissions: JObjectArray<'local>,
    results: JIntArray<'local>,
) {
    // SAFETY: Java only calls this with the live handle it received from `attach`.
    let Some(inner) = (unsafe { inner_from(handle) }) else { return };
    let outcome = env.with_env(|env| -> Result<bool, Error> {
        let mut grants = vec![0; results.len(env)?];
        results.get_region(env, 0, &mut grants)?;
        // An empty result means the request was interrupted.
        Ok(!grants.is_empty() && grants.iter().all(|&g| g == inner.permission_granted))
    });
    match outcome.into_outcome() {
        Outcome::Ok(granted) => inner.complete(request_code, granted),
        Outcome::Err(e) => {
            tracing::error!(request_code, "reading permission result: {e}");
            inner.complete(request_code, false);
        }
        Outcome::Panic(_) => {
            tracing::error!(request_code, "panic reading permission result");
            inner.complete(request_code, false);
        }
    }
}

extern "system" fn native_detach<'local>(_env: EnvUnowned<'local>, _class: JClass<'local>, handle: jlong) {
    let Some(address) = address_from_handle(handle) else { return };
    // SAFETY: Java calls this exactly once with the handle from `attach` and clears its copy first.
    drop(unsafe { Arc::from_raw(std::ptr::with_exposed_provenance::<Inner>(address)) });
    tracing::debug!("platform bridge detached");
}

fn permission_string<'local>(env: &mut Env<'local>, permission: Permission) -> Result<JObject<'local>, Error> {
    Ok(env
        .get_static_field(jni_str!("android/Manifest$permission"), permission.manifest_field(), jni_sig!("Ljava/lang/String;"))?
        .l()?)
}

fn exit_reason(env: &mut Env, name: &JNIStr) -> Result<i32, Error> {
    Ok(env.get_static_field(jni_str!("android/app/ApplicationExitInfo"), name, jni_sig!("I"))?.i()?)
}

fn trace_excerpt(env: &mut Env, info: &JObject) -> Result<String, Error> {
    let stream = env.call_method(info, jni_str!("getTraceInputStream"), jni_sig!("()Ljava/io/InputStream;"), &[])?.l()?;
    if stream.is_null() {
        return Ok(String::new());
    }
    let bytes = env.call_method(&stream, jni_str!("readAllBytes"), jni_sig!("()[B"), &[])?.l()?;
    let bytes = env.convert_byte_array(env.cast_local::<JByteArray>(bytes)?)?;
    Ok(printable_runs(&bytes))
}

fn java_string(env: &mut Env, obj: JObject) -> Result<String, Error> {
    let s = env.cast_local::<JString>(obj)?;
    if s.is_null() { Ok(String::new()) } else { Ok(s.try_to_string(env)?) }
}

/// Printable ASCII runs of a binary trace: frames, abort message, etc.
fn printable_runs(bytes: &[u8]) -> String {
    bytes
        .split(|b| !(b.is_ascii_graphic() || *b == b' '))
        .filter(|run| run.len() >= TRACE_MIN_RUN)
        .take(TRACE_MAX_LINES)
        .map(|run| format!("  {}\n", String::from_utf8_lossy(run)))
        .collect()
}
