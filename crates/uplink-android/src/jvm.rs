//! JNI helpers on the NativeActivity: permissions and previous-exit diagnostics.
//! Android constants are read from their Java classes, never hardcoded.

use android_activity::AndroidApp;
use jni::objects::{JByteArray, JObject, JString, JValue};
use jni::strings::JNIStr;
use jni::{Env, JavaVM, jni_sig, jni_str};

use crate::Error;

const PERMISSION_REQUEST_CODE: i32 = 1;
/// `getHistoricalProcessExitReasons` pid filter: all processes of the package.
const ALL_PIDS: i32 = 0;
const EXIT_RECORDS: i32 = 3;
const TRACE_MIN_RUN: usize = 6;
const TRACE_MAX_LINES: usize = 30;

fn with_activity<T>(app: &AndroidApp, f: impl FnOnce(&mut Env, &JObject) -> Result<T, Error>) -> Result<T, Error> {
    let vm = JavaVM::singleton()
        // SAFETY: android-activity guarantees a valid JavaVM pointer for the process lifetime.
        .unwrap_or_else(|_| unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) });
    vm.attach_current_thread(|env| {
        // SAFETY: a global ref to the activity owned by android-activity; not deleted here.
        let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
        f(env, &activity)
    })
}

fn camera_permission<'local>(env: &mut Env<'local>) -> Result<JObject<'local>, Error> {
    Ok(env
        .get_static_field(jni_str!("android/Manifest$permission"), jni_str!("CAMERA"), jni_sig!("Ljava/lang/String;"))?
        .l()?)
}

pub fn has_camera_permission(app: &AndroidApp) -> Result<bool, Error> {
    with_activity(app, |env, activity| {
        let perm = camera_permission(env)?;
        let granted = env
            .get_static_field(jni_str!("android/content/pm/PackageManager"), jni_str!("PERMISSION_GRANTED"), jni_sig!("I"))?
            .i()?;
        let state = env
            .call_method(activity, jni_str!("checkSelfPermission"), jni_sig!("(Ljava/lang/String;)I"), &[JValue::Object(&perm)])?
            .i()?;
        Ok(state == granted)
    })
}

pub fn request_camera_permission(app: &AndroidApp) -> Result<(), Error> {
    with_activity(app, |env, activity| {
        let perm = camera_permission(env)?;
        let perms = env.new_object_array(1, jni_str!("java/lang/String"), &perm)?;
        env.call_method(
            activity,
            jni_str!("requestPermissions"),
            jni_sig!("([Ljava/lang/String;I)V"),
            &[JValue::Object(&perms), JValue::Int(PERMISSION_REQUEST_CODE)],
        )?;
        Ok(())
    })
}

/// Recent `ApplicationExitInfo` records, with printable excerpts of crash/ANR traces
/// (native crashes carry a tombstone protobuf).
pub fn previous_exits(app: &AndroidApp) -> Result<String, Error> {
    with_activity(app, |env, activity| {
        let traced = [
            exit_reason(env, jni_str!("REASON_CRASH"))?,
            exit_reason(env, jni_str!("REASON_CRASH_NATIVE"))?,
            exit_reason(env, jni_str!("REASON_ANR"))?,
        ];
        let service = env.get_static_field(
            jni_str!("android/content/Context"),
            jni_str!("ACTIVITY_SERVICE"),
            jni_sig!("Ljava/lang/String;"),
        )?;
        let service = service.l()?;
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
