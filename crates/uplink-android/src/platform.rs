//! Java ↔ Rust bridge on `dev.uplink.UplinkActivity`.
//!
//! [`Platform::attach`] registers the natives and hands the activity a `jlong` handle to an
//! `Arc<Inner>`; Java passes it back on each callback and releases it in `onDestroy`. No globals:
//! `Platform` owns its `JavaVM` and a global ref to the activity. Android constants are read from
//! their Java classes.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use android_activity::AndroidApp;
use arc_swap::ArcSwapOption;
use jni::objects::{JByteArray, JClass, JIntArray, JObject, JObjectArray, JString, JValue};
use jni::refs::Global;
use jni::strings::JNIStr;
use jni::sys::{jint, jlong};
use jni::{Env, EnvUnowned, JavaVM, NativeMethod, Outcome, jni_sig, jni_str};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use tokio::sync::{mpsc, oneshot};

use crate::Error;
use crate::codec::Avc;

/// `getHistoricalProcessExitReasons` pid filter: all processes of the package.
const ALL_PIDS: i32 = 0;
const EXIT_RECORDS: i32 = 3;
const TRACE_MIN_RUN: usize = 6;
const TRACE_MAX_LINES: usize = 30;
const SINGLE_PERMISSION: i32 = 1;
/// Must match `UplinkFiles.DIRECTORY`, which is the Java side of the same agreement.
const SHARE_DIR: &str = "share";
/// `Build.VERSION_CODES.TIRAMISU`: the first Android with a notification permission. It cannot be
/// read off the device the way the other constants are — a platform older than this has no such
/// field, which is the whole problem being worked around.
const NOTIFICATIONS_SDK: jint = 33;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Camera,
    RecordAudio,
    /// Only for showing the call notification; the service runs either way.
    PostNotifications,
}

impl Permission {
    /// Field of `android.Manifest.permission` holding the permission string.
    const fn manifest_field(self) -> &'static JNIStr {
        match self {
            Self::Camera => jni_str!("CAMERA"),
            Self::RecordAudio => jni_str!("RECORD_AUDIO"),
            Self::PostNotifications => jni_str!("POST_NOTIFICATIONS"),
        }
    }
}

/// A picked image as greyscale pixels, which is all a QR decoder needs.
pub struct Greyscale {
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// State shared with Java through the activity's handle.
struct Inner {
    permission_granted: jint,
    next_request_code: AtomicI32,
    pending: Mutex<FxHashMap<jint, oneshot::Sender<bool>>>,
    pending_images: Mutex<FxHashMap<jint, oneshot::Sender<Option<Greyscale>>>>,
    /// Unsolicited platform events, answered on the UI loop.
    events: mpsc::UnboundedSender<PlatformEvent>,
}

impl Inner {
    fn complete(&self, request_code: jint, granted: bool) {
        let sender = self.pending.lock().remove(&request_code);
        match sender {
            // The receiver may have been dropped by a caller that stopped waiting.
            Some(tx) => drop(tx.send(granted)),
            None => tracing::warn!(request_code, "permission result for unknown request"),
        }
    }

    /// Fails only once the app is shutting down, when nothing is left to answer it.
    fn send(&self, event: PlatformEvent) {
        let _ = self.events.send(event);
    }

    fn complete_image(&self, request_code: jint, image: Option<Greyscale>) {
        let sender = self.pending_images.lock().remove(&request_code);
        match sender {
            Some(tx) => drop(tx.send(image)),
            None => tracing::warn!(request_code, "image result for unknown request"),
        }
    }
}

/// The `Application` and the VM to reach it with: everything the app can do with no window.
/// It exists before any activity, service or receiver, so permissions can be read through it,
/// audio routed, services started and notifications posted — everything a call needs before
/// anyone is looking at it. Cheap to clone; the process-wide core holds one.
#[derive(Clone)]
pub struct AppContext {
    vm: JavaVM,
    application: Arc<Global<JObject<'static>>>,
}

impl AppContext {
    /// From a native method called on the Application itself.
    pub fn new(env: &mut Env, application: &JObject) -> Result<Self, Error> {
        Ok(Self { vm: env.get_java_vm()?, application: Arc::new(env.new_global_ref(application)?) })
    }

    /// The one place a throw is still pending, so it is where Java's own message is read.
    fn with<T>(&self, f: impl FnOnce(&mut Env, &JObject) -> Result<T, Error>) -> Result<T, Error> {
        self.vm.attach_current_thread(|env| match f(env, self.application.as_obj()) {
            Err(Error::Jni(jni::errors::Error::JavaException)) => Err(thrown(env)),
            outcome => outcome,
        })
    }

    /// Rings for an incoming call: the user's ringtone, and a call notification whenever the
    /// app is not in front. Stopped by [`Self::stop_ringing`], not by a timer of its own.
    pub fn ring(&self, who: &str) -> Result<(), Error> {
        self.with(|env, application| {
            let who = env.new_string(who)?;
            env.call_method(application, jni_str!("ring"), jni_sig!("(Ljava/lang/String;)V"), &[JValue::Object(&who)])?;
            Ok(())
        })
    }

    pub fn stop_ringing(&self) -> Result<(), Error> {
        self.call(jni_str!("stopRinging"))
    }

    /// A no-argument `void` method of the Application.
    fn call(&self, method: &JNIStr) -> Result<(), Error> {
        self.with(|env, application| {
            env.call_method(application, method, jni_sig!("()V"), &[])?;
            Ok(())
        })
    }

    /// A no-argument `boolean` method of the Application.
    fn ask(&self, method: &JNIStr) -> Result<bool, Error> {
        self.with(|env, application| Ok(env.call_method(application, method, jni_sig!("()Z"), &[])?.z()?))
    }
}

pub struct Platform {
    context: AppContext,
    /// Only while an activity is. Requesting a permission, picking an image, the window's system
    /// bars, picture-in-picture and stepping into the background all need one, and none of them
    /// can happen without someone looking at the screen anyway.
    ///
    /// Read on nearly every platform call, from whichever thread is making it, and replaced twice
    /// in an activity's life — so it is swapped rather than locked, and a reader takes its own
    /// reference without ever blocking the one that replaces it. `Global` cannot be cloned
    /// without an `Env`, a new global ref being a JNI call, so the refcount is an `Arc` of ours.
    activity: ArcSwapOption<Global<JObject<'static>>>,
    inner: Arc<Inner>,
    /// `Build.VERSION.SDK_INT`. What the app was built against says nothing about what it is
    /// running on, and the two disagree by three years on a phone that cannot take Google's
    /// updates — so anything newer than `minSdk` is asked about here before it is used.
    sdk: jint,
}

/// Something the platform did that nobody asked for. The codes match `UplinkActivity`'s own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformEvent {
    /// The call window shrank into, or grew out of, picture-in-picture.
    PictureInPicture(bool),
    Hangup,
    ToggleMic,
    /// Answer was tapped on the ringing notification, which opened this window to take it.
    Answer,
}

impl PlatformEvent {
    const fn action(code: jint) -> Option<Self> {
        match code {
            0 => Some(Self::Hangup),
            1 => Some(Self::ToggleMic),
            2 => Some(Self::Answer),
            _ => None,
        }
    }
}

pub type PlatformEvents = mpsc::UnboundedReceiver<PlatformEvent>;

impl Platform {
    pub fn attach(app: &AndroidApp) -> Result<(Self, PlatformEvents), Error> {
        // SAFETY: android-activity guarantees a valid JavaVM pointer for the process lifetime.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let (events, incoming) = mpsc::unbounded_channel();
        let (activity, context, inner, sdk) = vm.attach_current_thread(|env| -> Result<_, Error> {
            // SAFETY: a global ref to the activity owned by android-activity; not deleted here.
            let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
            let class = env.get_object_class(&activity)?;
            // SAFETY: the function pointers match the Java declarations in UplinkActivity.
            unsafe { env.register_native_methods(&class, &natives())? };
            let inner = Arc::new(Inner {
                permission_granted: env
                    .get_static_field(
                        jni_str!("android/content/pm/PackageManager"),
                        jni_str!("PERMISSION_GRANTED"),
                        jni_sig!("I"),
                    )?
                    .i()?,
                next_request_code: AtomicI32::new(0),
                pending: Mutex::default(),
                pending_images: Mutex::default(),
                events,
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
            let sdk = env
                .get_static_field(jni_str!("android/os/Build$VERSION"), jni_str!("SDK_INT"), jni_sig!("I"))?
                .i()?;
            // The Application outlives this activity and is what everything headless runs on.
            // Asking the activity for it avoids `FindClass`, which on a thread attached from
            // native code searches the system classloader and has never heard of us.
            let application = env
                .call_method(&activity, jni_str!("getApplication"), jni_sig!("()Landroid/app/Application;"), &[])?
                .l()?;
            Ok((env.new_global_ref(&activity)?, AppContext::new(env, &application)?, inner, sdk))
        })?;
        // Not logged here: attaching is what finds this process's core, and therefore happens
        // before there is a subscriber to log to. The caller reports `sdk` once it does — see
        // [`Self::sdk`].
        let activity = ArcSwapOption::from(Some(Arc::new(activity)));
        Ok((Self { context, activity, inner, sdk }, incoming))
    }

    /// Anything that only needs a `Context`, which is everything the app does when nobody is
    /// looking at it.
    fn with_context<T>(&self, f: impl FnOnce(&mut Env, &JObject) -> Result<T, Error>) -> Result<T, Error> {
        self.context.with(f)
    }

    /// Anything that needs the window or the task. Fails plainly when there is no activity rather
    /// than pretending, because the caller is asking for something only a visible app can do.
    fn with_activity<T>(&self, f: impl FnOnce(&mut Env, &JObject) -> Result<T, Error>) -> Result<T, Error> {
        let activity = self.activity.load_full().ok_or(Error::NoActivity)?;
        self.context.vm.attach_current_thread(|env| match f(env, activity.as_obj()) {
            Err(Error::Jni(jni::errors::Error::JavaException)) => Err(thrown(env)),
            outcome => outcome,
        })
    }

    /// Whether this Android has a notification permission at all. Before 13 there is none to
    /// hold: an app may post unless the user has switched its notifications off. Asking anyway
    /// does not merely come back false — `POST_NOTIFICATIONS` is not a field of that platform's
    /// `Manifest.permission`, so looking it up by name throws, and the app reads its own
    /// notifications as refused for good however many times the user grants them.
    const fn asks_about_notifications(&self) -> bool {
        self.sdk >= NOTIFICATIONS_SDK
    }

    /// Whether the app may post notifications, which is the whole of the answer before 13 and
    /// agrees with the permission after it.
    fn notifications_enabled(&self) -> Result<bool, Error> {
        self.with_context(|env, context| {
            Ok(env.call_method(context, jni_str!("notificationsEnabled"), jni_sig!("()Z"), &[])?.z()?)
        })
    }

    pub fn has_permission(&self, permission: Permission) -> Result<bool, Error> {
        if permission == Permission::PostNotifications && !self.asks_about_notifications() {
            return self.notifications_enabled();
        }
        let granted = self.inner.permission_granted;
        self.with_context(|env, context| {
            let name = permission_string(env, permission)?;
            let state = env
                .call_method(
                    context,
                    jni_str!("checkSelfPermission"),
                    jni_sig!("(Ljava/lang/String;)I"),
                    &[JValue::Object(&name)],
                )?
                .i()?;
            Ok(state == granted)
        })
    }

    /// Whether Android would still show its own dialog. False once the user has refused for good,
    /// which is the only case that has to send them to Settings.
    pub fn should_explain(&self, permission: Permission) -> Result<bool, Error> {
        // No dialog exists to explain, so this is the "only Settings can fix it" case outright.
        if permission == Permission::PostNotifications && !self.asks_about_notifications() {
            return Ok(false);
        }
        self.with_activity(|env, activity| {
            let name = permission_string(env, permission)?;
            Ok(env
                .call_method(
                    activity,
                    jni_str!("shouldExplain"),
                    jni_sig!("(Ljava/lang/String;)Z"),
                    &[JValue::Object(&name)],
                )?
                .z()?)
        })
    }

    /// Tells the status and navigation bars which way their icons should go. The app draws under
    /// them, so what is behind them is the app's own ground and only the app knows its shade.
    pub fn set_light_system_bars(&self, light: bool) -> Result<(), Error> {
        self.with_activity(|env, activity| {
            env.call_method(
                activity,
                jni_str!("setLightSystemBars"),
                jni_sig!("(Z)V"),
                &[JValue::Bool(light)],
            )?;
            Ok(())
        })
    }

    /// What the mic is doing, so the buttons that can reach it offer the move the user has not
    /// made yet: the notification always, and the shrunken window when there is one.
    pub fn set_mic_on(&self, on: bool) -> Result<(), Error> {
        self.with_context(|env, context| {
            env.call_method(context, jni_str!("setMicOn"), jni_sig!("(Z)V"), &[JValue::Bool(on)])?;
            Ok(())
        })?;
        self.on_screen(|env, activity| {
            env.call_method(activity, jni_str!("refreshPictureInPicture"), jni_sig!("()V"), &[])?;
            Ok(())
        })
    }

    /// For the half of a change that only matters while the app is visible: a missing activity is
    /// the answer, not a failure.
    fn on_screen(&self, f: impl FnOnce(&mut Env, &JObject) -> Result<(), Error>) -> Result<(), Error> {
        match self.with_activity(f) {
            Err(Error::NoActivity) => Ok(()),
            outcome => outcome,
        }
    }

    /// Keeps the process alive so the endpoint stays bound. Without it, swiping the app out of
    /// recents takes the whole process, and with it anything a call could arrive at.
    pub fn set_listening(&self, listening: bool) -> Result<(), Error> {
        self.with_context(|env, context| {
            env.call_method(context, jni_str!("setListening"), jni_sig!("(Z)V"), &[JValue::Bool(listening)])?;
            tracing::info!(listening, "reachable in the background");
            Ok(())
        })
    }

    /// The process-wide handle to whatever outlives the screen, started now if nothing has
    /// started it yet — which blocks until the endpoint is bound. Zero if starting it failed.
    pub fn core_handle(&self) -> Result<usize, Error> {
        self.with_context(|env, context| {
            let handle = env.call_method(context, jni_str!("ensureCore"), jni_sig!("()J"), &[])?.j()?;
            Ok(address_from_handle(handle).unwrap_or_default())
        })
    }

    /// Whether Android's battery optimisation leaves the app alone, which is what lets the
    /// endpoint stay bound through the night.
    pub fn battery_unrestricted(&self) -> Result<bool, Error> {
        self.context.ask(jni_str!("batteryUnrestricted"))
    }

    /// Opens Android's dialog from the activity, so it sits over our own screen rather than
    /// Settings', and resolves once it closes — to whether the app is unrestricted now, read from
    /// Android rather than from the dialog's result code.
    pub async fn request_battery_exemption(&self) -> Result<bool, Error> {
        let code = self.inner.next_request_code.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().insert(code, tx);
        let opened = self.with_activity(|env, activity| {
            env.call_method(activity, jni_str!("requestBatteryExemption"), jni_sig!("(I)V"), &[JValue::Int(code)])?;
            Ok(())
        });
        if let Err(e) = opened {
            self.inner.pending.lock().remove(&code);
            return Err(e);
        }
        rx.await.map_err(|_| Error::RequestAbandoned)?;
        self.battery_unrestricted()
    }

    /// Whether a ringing call may take over the screen; only ever false from Android 14.
    pub fn full_screen_calls_allowed(&self) -> Result<bool, Error> {
        self.context.ask(jni_str!("fullScreenCallsAllowed"))
    }

    pub fn open_full_screen_calls_settings(&self) -> Result<(), Error> {
        self.context.call(jni_str!("openFullScreenCallsSettings"))
    }

    /// Whether this phone's maker keeps its own list of apps it may stop, and lets us open it.
    /// There is no reading what is on that list, only offering it.
    pub fn has_maker_list(&self) -> Result<bool, Error> {
        self.context.ask(jni_str!("hasMakerList"))
    }

    pub fn open_maker_list(&self) -> Result<(), Error> {
        self.context.call(jni_str!("openMakerList"))
    }

    /// Steps the app behind whatever else is on screen, without tearing it down — the process has
    /// to stay up, because an endpoint that is gone cannot be rung.
    pub fn move_to_background(&self) -> Result<(), Error> {
        self.with_activity(|env, activity| {
            env.call_method(activity, jni_str!("moveToBackground"), jni_sig!("()V"), &[])?;
            Ok(())
        })
    }

    /// Opens this app's own page in Settings, where a permission refused for good can be granted.
    pub fn open_app_settings(&self) -> Result<(), Error> {
        self.with_context(|env, context| {
            env.call_method(context, jni_str!("openAppSettings"), jni_sig!("()V"), &[])?;
            Ok(())
        })
    }

    /// Resolves to whether the permission is granted, prompting the user if needed.
    pub async fn request_permission(&self, permission: Permission) -> Result<bool, Error> {
        if self.has_permission(permission)? {
            return Ok(true);
        }
        // Switched off, and there is no dialog on this platform that could switch them back on.
        if permission == Permission::PostNotifications && !self.asks_about_notifications() {
            return Ok(false);
        }
        let code = self.inner.next_request_code.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().insert(code, tx);
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
            self.inner.pending.lock().remove(&code);
            return Err(e);
        }
        tracing::debug!(?permission, code, "permission requested");
        rx.await.map_err(|_| Error::RequestAbandoned)
    }

    /// Puts the device in (or out of) a voice call: routes to the call stream and enables the
    /// platform's echo cancellation, with the speaker on for video calls.
    pub fn set_in_call(&self, in_call: bool) -> Result<(), Error> {
        self.with_context(|env, context| {
            let mode = audio_mode(env, if in_call { jni_str!("MODE_IN_COMMUNICATION") } else { jni_str!("MODE_NORMAL") })?;
            let manager = audio_manager(env, context)?;
            env.call_method(&manager, jni_str!("setMode"), jni_sig!("(I)V"), &[JValue::Int(mode)])?;
            env.call_method(
                &manager,
                jni_str!("setSpeakerphoneOn"),
                jni_sig!("(Z)V"),
                &[JValue::Bool(in_call)],
            )?;
            tracing::debug!(in_call, "audio mode set");
            Ok(())
        })
    }

    /// Opens the system image picker; resolves to the chosen image, or `None` if the user backed
    /// out or it could not be read.
    pub async fn pick_image(&self) -> Result<Option<Greyscale>, Error> {
        let code = self.inner.next_request_code.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending_images.lock().insert(code, tx);
        let opened = self.with_activity(|env, activity| {
            env.call_method(activity, jni_str!("pickImageAsync"), jni_sig!("(I)V"), &[JValue::Int(code)])?;
            Ok(())
        });
        if let Err(e) = opened {
            self.inner.pending_images.lock().remove(&code);
            return Err(e);
        }
        rx.await.map_err(|_| Error::RequestAbandoned)
    }

    /// Offers a file already written into [`Self::share_dir`] to the share sheet.
    pub fn share_file(&self, name: &str, title: &str) -> Result<(), Error> {
        self.with_context(|env, context| {
            let (name, title) = (env.new_string(name)?, env.new_string(title)?);
            env.call_method(
                context,
                jni_str!("shareFile"),
                jni_sig!("(Ljava/lang/String;Ljava/lang/String;)V"),
                &[JValue::Object(&name), JValue::Object(&title)],
            )?;
            Ok(())
        })
    }

    /// Where a file has to be for [`Self::share_image`] to find it: the one directory the app's
    /// content provider serves, inside the data directory. Nothing else belongs in it.
    pub fn share_dir(data_dir: &Path) -> PathBuf {
        data_dir.join(SHARE_DIR)
    }

    /// Speaker or earpiece for the call audio.
    pub fn set_speaker(&self, on: bool) -> Result<(), Error> {
        self.with_context(|env, context| {
            let manager = audio_manager(env, context)?;
            env.call_method(&manager, jni_str!("setSpeakerphoneOn"), jni_sig!("(Z)V"), &[JValue::Bool(on)])?;
            tracing::debug!(on, "speakerphone");
            Ok(())
        })
    }

    /// Runs the foreground service that lets a call keep the camera and microphone while the app
    /// is in the background. `peer` is who the call's notification names.
    pub fn set_call_service(&self, running: bool, peer: &str) -> Result<(), Error> {
        self.with_context(|env, context| {
            let peer = env.new_string(peer)?;
            env.call_method(
                context,
                jni_str!("setCall"),
                jni_sig!("(ZLjava/lang/String;)V"),
                &[JValue::Bool(running), JValue::Object(&peer)],
            )?;
            tracing::debug!(running, "call service");
            Ok(())
        })?;
        // The window's half: arming the shrink, and dismissing it when there is nothing to watch.
        self.on_screen(|env, activity| {
            env.call_method(activity, jni_str!("setInCall"), jni_sig!("(Z)V"), &[JValue::Bool(running)])?;
            Ok(())
        })
    }

    /// Which Android this is. It decides what the app may ask for, and it is the first thing
    /// worth knowing about a device that behaves oddly, so the caller logs it at startup.
    pub const fn sdk(&self) -> jint {
        self.sdk
    }

    /// H.264 codec constants from the SDK (the NDK headers don't carry them).
    pub fn avc(&self) -> Result<Avc, Error> {
        self.with_context(|env, _| {
            let mime = env
                .get_static_field(
                    jni_str!("android/media/MediaFormat"),
                    jni_str!("MIMETYPE_VIDEO_AVC"),
                    jni_sig!("Ljava/lang/String;"),
                )?
                .l()?;
            let surface_color_format = env
                .get_static_field(
                    jni_str!("android/media/MediaCodecInfo$CodecCapabilities"),
                    jni_str!("COLOR_FormatSurface"),
                    jni_sig!("I"),
                )?
                .i()?;
            Ok(Avc { mime: java_string(env, mime)?, surface_color_format })
        })
    }

    /// Recent `ApplicationExitInfo` records, with printable excerpts of crash/ANR traces
    /// (native crashes carry a tombstone protobuf).
    pub fn previous_exits(&self) -> Result<String, Error> {
        self.with_context(|env, context| {
            let traced = [
                exit_reason(env, jni_str!("REASON_CRASH"))?,
                exit_reason(env, jni_str!("REASON_CRASH_NATIVE"))?,
                exit_reason(env, jni_str!("REASON_ANR"))?,
            ];
            let service = env
                .get_static_field(
                    jni_str!("android/content/Context"),
                    jni_str!("ACTIVITY_SERVICE"),
                    jni_sig!("Ljava/lang/String;"),
                )?
                .l()?;
            let am = env
                .call_method(
                    context,
                    jni_str!("getSystemService"),
                    jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
                    &[JValue::Object(&service)],
                )?
                .l()?;
            let package =
                env.call_method(context, jni_str!("getPackageName"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
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
                let info = env
                    .call_method(&list, jni_str!("get"), jni_sig!("(I)Ljava/lang/Object;"), &[JValue::Int(i)])?
                    .l()?;
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

fn natives() -> [NativeMethod<'static>; 6] {
    // SAFETY: signatures match the `extern "system"` functions below and UplinkActivity's natives.
    unsafe {
        [
            NativeMethod::from_raw_parts(
                jni_str!("nativePermissionsResult"),
                jni_str!("(JI[Ljava/lang/String;[I)V"),
                native_permissions_result as *mut c_void,
            ),
            NativeMethod::from_raw_parts(
                jni_str!("nativeImagePicked"),
                jni_str!("(JI[III)V"),
                native_image_picked as *mut c_void,
            ),
            NativeMethod::from_raw_parts(jni_str!("nativeDetach"), jni_str!("(J)V"), native_detach as *mut c_void),
            NativeMethod::from_raw_parts(
                jni_str!("nativePictureInPicture"),
                jni_str!("(JZ)V"),
                native_picture_in_picture as *mut c_void,
            ),
            NativeMethod::from_raw_parts(
                jni_str!("nativeCallAction"),
                jni_str!("(JI)V"),
                native_call_action as *mut c_void,
            ),
            NativeMethod::from_raw_parts(
                jni_str!("nativeActivityDone"),
                jni_str!("(JI)V"),
                native_activity_done as *mut c_void,
            ),
        ]
    }
}

/// Bit-preserving pointer → `jlong`: arm64 Android tags heap pointers in the top byte, so
/// addresses routinely exceed `i64::MAX` as unsigned values.
pub fn handle_from_address(address: usize) -> Result<jlong, Error> {
    jlong::try_from(address.cast_signed()).map_err(|_| Error::Handle)
}

pub fn address_from_handle(handle: jlong) -> Option<usize> {
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
    let Some(inner) = (unsafe { inner_from(handle) }) else {
        return;
    };
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

/// The picked image, as packed ARGB from `Bitmap.getPixels`, reduced to the luma a QR decoder
/// reads.
extern "system" fn native_image_picked<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    request_code: jint,
    pixels: JIntArray<'local>,
    width: jint,
    height: jint,
) {
    // SAFETY: Java only calls this with the live handle it received from `attach`.
    let Some(inner) = (unsafe { inner_from(handle) }) else { return };
    let outcome = env.with_env(|env| -> Result<Option<Greyscale>, Error> {
        let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else {
            return Ok(None);
        };
        if pixels.is_null() || width == 0 || height == 0 {
            return Ok(None);
        }
        let mut argb = vec![0; pixels.len(env)?];
        pixels.get_region(env, 0, &mut argb)?;
        Ok((argb.len() >= width * height).then(|| Greyscale { pixels: luma(&argb), width, height }))
    });
    match outcome.into_outcome() {
        Outcome::Ok(image) => inner.complete_image(request_code, image),
        Outcome::Err(e) => {
            tracing::error!(request_code, "reading picked image: {e}");
            inner.complete_image(request_code, None);
        }
        Outcome::Panic(_) => {
            tracing::error!(request_code, "panic reading picked image");
            inner.complete_image(request_code, None);
        }
    }
}

/// ITU-R BT.601 luma, the weighting Android's own greyscale conversions use.
fn luma(argb: &[jint]) -> Vec<u8> {
    const RED: i32 = 299;
    const GREEN: i32 = 587;
    const BLUE: i32 = 114;
    const TOTAL: i32 = RED + GREEN + BLUE;
    const BYTE: i32 = 0xff;
    argb.iter()
        .map(|pixel| {
            let (r, g, b) = ((pixel >> 16) & BYTE, (pixel >> 8) & BYTE, pixel & BYTE);
            u8::try_from((r * RED + g * GREEN + b * BLUE) / TOTAL).unwrap_or(u8::MAX)
        })
        .collect()
}

extern "system" fn native_detach<'local>(_env: EnvUnowned<'local>, _class: JClass<'local>, handle: jlong) {
    let Some(address) = address_from_handle(handle) else {
        return;
    };
    // SAFETY: Java calls this exactly once with the handle from `attach` and clears its copy first.
    drop(unsafe { Arc::from_raw(std::ptr::with_exposed_provenance::<Inner>(address)) });
    tracing::debug!("platform bridge detached");
}

extern "system" fn native_picture_in_picture<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    active: bool,
) {
    // SAFETY: Java passes back the handle from `attach` and clears it before detaching.
    let Some(inner) = (unsafe { inner_from(handle) }) else {
        return;
    };
    inner.send(PlatformEvent::PictureInPicture(active));
}

extern "system" fn native_call_action<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    action: jint,
) {
    // SAFETY: as above.
    let Some(inner) = (unsafe { inner_from(handle) }) else {
        return;
    };
    match PlatformEvent::action(action) {
        Some(event) => inner.send(event),
        None => tracing::warn!(action, "unknown call action"),
    }
}

/// A screen started for a result has closed. Only that it closed is passed on: what it changed
/// is read back from Android by whoever was waiting.
extern "system" fn native_activity_done<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    request_code: jint,
) {
    // SAFETY: as above.
    let Some(inner) = (unsafe { inner_from(handle) }) else {
        return;
    };
    inner.complete(request_code, true);
}

fn permission_string<'local>(env: &mut Env<'local>, permission: Permission) -> Result<JObject<'local>, Error> {
    Ok(env
        .get_static_field(
            jni_str!("android/Manifest$permission"),
            permission.manifest_field(),
            jni_sig!("Ljava/lang/String;"),
        )?
        .l()?)
}

fn audio_manager<'local>(env: &mut Env<'local>, activity: &JObject) -> Result<JObject<'local>, Error> {
    let service = env
        .get_static_field(jni_str!("android/content/Context"), jni_str!("AUDIO_SERVICE"), jni_sig!("Ljava/lang/String;"))?
        .l()?;
    Ok(env
        .call_method(
            activity,
            jni_str!("getSystemService"),
            jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
            &[JValue::Object(&service)],
        )?
        .l()?)
}

fn audio_mode(env: &mut Env, name: &JNIStr) -> Result<i32, Error> {
    Ok(env.get_static_field(jni_str!("android/media/AudioManager"), name, jni_sig!("I"))?.i()?)
}

fn exit_reason(env: &mut Env, name: &JNIStr) -> Result<i32, Error> {
    Ok(env.get_static_field(jni_str!("android/app/ApplicationExitInfo"), name, jni_sig!("I"))?.i()?)
}

fn trace_excerpt(env: &mut Env, info: &JObject) -> Result<String, Error> {
    let stream =
        env.call_method(info, jni_str!("getTraceInputStream"), jni_sig!("()Ljava/io/InputStream;"), &[])?.l()?;
    if stream.is_null() {
        return Ok(String::new());
    }
    let bytes = env.call_method(&stream, jni_str!("readAllBytes"), jni_sig!("()[B"), &[])?.l()?;
    let bytes = env.convert_byte_array(env.cast_local::<JByteArray>(bytes)?)?;
    Ok(printable_runs(&bytes))
}

/// Reads the pending exception's `toString()` — which is the class and the message, e.g.
/// `java.io.FileNotFoundException: no such shared file` — and clears it, so the next JNI call on
/// this thread is not refused by an exception nobody handled. Anything that goes wrong while
/// reading it is reported as itself rather than replacing the throw with a second failure.
fn thrown(env: &mut Env) -> Error {
    let Some(throwable) = env.exception_occurred() else {
        return Error::Jni(jni::errors::Error::JavaException);
    };
    // Cleared before calling back into Java: a pending exception makes every further call fail.
    env.exception_clear();
    let described = env
        .call_method(&throwable, jni_str!("toString"), jni_sig!("()Ljava/lang/String;"), &[])
        .and_then(|text| text.l())
        .map_err(Error::from)
        .and_then(|text| java_string(env, text));
    match described {
        Ok(text) => Error::Thrown(text),
        Err(e) => {
            tracing::warn!("reading a java exception: {e}");
            Error::Jni(jni::errors::Error::JavaException)
        }
    }
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
