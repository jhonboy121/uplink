# jni 0.22 — API reference (verified)

Quick reference for the `jni` crate **0.22.x** (the version Slint 1.18's Android
backend pins). 0.22 is a big API break from 0.21: `JNIEnv` → `Env`, closures for
attachment, `jni_str!`/`jni_sig!` macros for names and signatures.

Source of truth: `~/.cargo/registry/src/*/jni-0.22.4/src/` (env.rs, vm/java_vm.rs,
jvalue.rs, objects/, refs/). Patterns marked ✅ compile in spike 2
(`cargo check`, 2026-09-22).

## Core types

| Type | What |
|---|---|
| `jni::JavaVM` | The VM. `JavaVM::singleton()` / `unsafe JavaVM::from_raw(*mut sys::JavaVM)` (from_raw also initialises the singleton) |
| `jni::Env<'local>` | Replaces 0.21 `JNIEnv`. You only get `&mut Env` inside closures |
| `jni::EnvUnowned<'local>` | First arg of `extern "system"` native methods |
| `jni::objects::{JObject, JString, JClass, JByteArray, JThrowable, JList, JMap, JByteBuffer, …}` | Typed local refs, lifetime-bound to the frame |
| `jni::refs::{Global<T>, Auto<'local, T>}` | Global ref (from `env.new_global_ref(obj)`), auto-deleting local |
| `jni::objects::JValue<'a>` | Argument: `Object(&JObject)`, `Byte`, `Char`, `Short`, `Int`, `Long`, `Bool`, `Float`, `Double`, `Void` |
| `JValueOwned<'local>` | Return value. Accessors: `.l()` → `JObject`, `.i()`, `.j()`, `.z()` (all `Result`) |
| `jni::errors::{Error, Result}` | Variants include `WrongObjectType`, `InvalidArgList`, `NullPtr`, `ClassNotFound{name}`, `JavaException` |

## Attaching / getting an `Env`

```rust
// ✅ Android (android-activity / Slint): reuse the singleton if Slint already made it.
let vm = JavaVM::singleton()
    .unwrap_or_else(|_| unsafe { JavaVM::from_raw(app.vm_as_ptr() as *mut _) });
vm.attach_current_thread(|env| -> jni::errors::Result<T> {
    // ... use env ...
    Ok(value)
})?;
```

- `attach_current_thread(F)` where `F: FnOnce(&mut Env) -> Result<T, E>`, `E: From<jni::errors::Error>`.
  Pushes a local frame (default capacity), so locals are freed on return.
- `attach_current_thread_for_scope`, `attach_current_thread_with_config(AttachConfig, cap, f)`,
  `detach_current_thread()` exist too.
- `env.with_local_frame(cap, |env| ...)` for loops that create many locals.

## Names and signatures

```rust
use jni::{jni_str, jni_sig};
jni_str!("getPackageName")                            // &JNIStr, compile-time checked
jni_sig!("(Ljava/lang/String;II)Ljava/util/List;")    // ✅ raw JNI signature
jni_sig!((str: JString) -> JString)                   // ergonomic form (from crate docs)
```
Also: `jni_cstr!`, `jni_sig_str!`, `jni_sig_cstr!`, `jni_sig_jstr!`, `jni_mangle`, `native_method!`, `bind_java_type!`.

Class names for `find_class` use slashes: `jni_str!("java/lang/String")`. Nested: `android/os/Build$VERSION`.

## Calling methods ✅

```rust
let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr() as jni::sys::jobject) };
let svc = env.new_string("activity")?;                                   // JString
let am = env.call_method(&activity, jni_str!("getSystemService"),
        jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
        &[JValue::Object(&svc)])?.l()?;                                   // JString derefs to &JObject
let n = env.call_method(&list, jni_str!("size"), jni_sig!("()I"), &[])?.i()?;
let ts = env.call_method(&info, jni_str!("getTimestamp"), jni_sig!("()J"), &[])?.j()?;
```

- `call_method(obj, name, sig, &[JValue])` checks arg count and primitive/object kinds at runtime
  (`Error::InvalidArgList` on mismatch). It does a class+method lookup on every call, so cache
  method IDs, or use `bind_java_type!` in hot paths.
- `call_static_method(class, name, sig, args)`, `new_object(class, sig, args)`,
  `get_field(obj, name, sig)`, `get_static_field(class, name, sig)`.
- Unchecked, faster: `call_method_unchecked` with a cached `JMethodID`.

## Casting and strings ✅

```rust
let s: JString = env.cast_local::<JString>(obj)?;   // runtime instanceof check; null → Ok(null)
if s.is_null() { /* … */ }
let rust: String = s.try_to_string(env)?;             // Err(NullPtr) if null
let js = env.new_string("hello")?;                    // or JString::new(env, "hello")?
let js2 = JString::from_str(env, "x")?;
// Upcast is free: let o: JObject = js.into();
```

Also `env.new_cast_local_ref::<T>(&obj)` and `env.as_cast::<T>(&obj)` (borrowing cast).
Slint uses `jstring.to_string()` via `Display`, which is fine when a valid `Env` is current.

## Arrays and buffers ✅

```rust
let arr = env.cast_local::<JByteArray>(obj)?;
let v: Vec<u8> = env.convert_byte_array(&arr)?;      // catches exceptions internally
let jarr = env.byte_array_from_slice(&bytes)?;
let jarr = env.new_byte_array(len)?;
arr.get_region(env, start, &mut buf)?;                 // ✅ JPrimitiveArray method; Env::get_*_array_region is deprecated
let ptr = env.get_direct_buffer_address(&byte_buffer)?;  // zero-copy for DirectByteBuffer
```
`arr.len(env)? -> usize` ✅ (`Env::get_array_length` is **deprecated** and clippy -D warnings rejects it). `get_object_array_element` also exists.

## References

- Locals die with the frame (closure return). In long loops use `with_local_frame`, or wrap them
  with `.auto()` (`Auto` deletes on drop), or call `env.delete_local_ref(obj)`.
- `env.new_global_ref(obj)?` gives `Global<T>`, which is Send and can be stored across calls and threads.
- `JObject::from_raw(env, raw)` is `unsafe`. It wraps a raw `jobject`, e.g. android-activity's
  `activity_as_ptr()` (a global ref owned by the glue, so don't delete it).
- `env.is_same_object(a, b)`.

## Exceptions

- A pending Java exception makes the call return `Err(Error::JavaException)`.
- `env.exception_check()`, `env.exception_occurred() -> Option<JThrowable>`,
  `env.exception_describe()` (prints to logcat), `env.exception_clear()`.
- `env.throw_new(class, msg)`, `env.throw_new_void(class)`.
- Native-method closures can map errors to Java exceptions with `EnvOutcome::resolve::<ThrowRuntimeExAndDefault>()`.

## Native methods (Java → Rust)

```rust
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_uplink_Foo_bar<'caller>(
    mut unowned_env: jni::EnvUnowned<'caller>,
    _this: JObject<'caller>,
    arg: JString<'caller>,
) -> JObject<'caller> {
    unowned_env.with_env(|env| -> jni::errors::Result<_> {
        Ok(JObject::null())
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}
```
`with_env` wraps the closure in `catch_unwind`, so panics don't unwind into the JVM.
Dynamic registration uses `NativeMethod` + `native_method!` (see env.rs ~L4897).

## `bind_java_type!` (typed bindings, what Slint uses)

```rust
bind_java_type! {
    AndroidContext => "android.content.Context",
    type_map = { JFile => "java.io.File" },
    methods {
        fn get_files_dir() -> JFile,
        fn get_package_name() -> JString,
    }
}
// Method blocks can also rename: fn set_clipboard { name = "set_clipboard", sig = (text: JString) },
// and bind `fields { … }` (e.g. "android.os.Build$VERSION").
```
Worked example: `i-slint-backend-android-activity-1.18.1/javahelper.rs` (lines ~20–200). It also
loads a dex through `dalvik.system.DexClassLoader` / `InMemoryDexClassLoader`.

## Gotchas

- `Env` has runtime "top frame" checks: keep one `&mut Env` per scope and don't smuggle an outer
  env into an inner frame, or it panics.
- `call_method` on a null object gives `Err(NullPtr)`. Check `.is_null()` on returned objects
  (e.g. `getTraceInputStream()` can be null).
- Android `getHistoricalProcessExitReasons(pkg, pid=0, max)` returns `java.util.List`. Iterate with
  `size()` / `get(I)Ljava/lang/Object;`. Reasons: 4 CRASH, 5 CRASH_NATIVE (trace is a tombstone
  protobuf on API 31+), 6 ANR (text trace). `InputStream.readAllBytes()` is API 33+.

## Registering natives without globals ✅ (uplink platform bridge)

- Don't use `native_method!`: its generated wrapper holds a `static Once` ABI check. Instead write plain
  `extern "system" fn f<'local>(mut env: EnvUnowned<'local>, _class: JClass<'local>, handle: jlong, …)` and register
  them via `NativeMethod::from_raw_parts(jni_str!("name"), jni_str!("(J…)V"), f as *mut c_void)` +
  `unsafe { env.register_native_methods(&class, &methods) }`, where `class = env.get_object_class(&activity)?`
  (the activity's own class; `FindClass` from a native thread can't see app classes).
- Context passing: `Arc::into_raw(Arc::clone(&inner)).expose_provenance()` → **`usize::cast_signed()`** → `jlong` stored in a Java `volatile long`
  (arm64 Android tags heap pointers in the top byte, so `jlong::try_from(usize)` fails on real devices; reverse with
  `isize::try_from(handle)?.cast_unsigned()`)
  field. Callbacks do `std::ptr::with_exposed_provenance::<Inner>(handle as usize).as_ref()`. `onDestroy` zeroes the
  field, then calls `nativeDetach`, which does `Arc::from_raw`.
- Inside native fns: `env.with_env(|env| …).into_outcome()` → `Outcome::{Ok, Err, Panic}`. Log and fall back
  rather than `resolve::<ThrowRuntimeExAndDefault>()`, because a Java exception thrown from a framework callback
  (e.g. `onRequestPermissionsResult`) crashes the app.
- Owned VM: `unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }` + `env.new_global_ref(&activity)?`
  (`Global<JObject<'static>>`, `.as_obj()`), no `JavaVM::singleton()`.
- Java side: `javac -source 8 -target 8 -bootclasspath android.jar` can't compile lambdas ("Unable to find method
  metafactory"). Use anonymous classes (`new Runnable() { … }`).
