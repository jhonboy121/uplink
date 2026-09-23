//! Camera2 NDK capture session streaming into one or more `ANativeWindow`s.

use std::ffi::{CStr, CString, c_int, c_void};
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use ndk::native_window::NativeWindow;
use ndk_sys as ffi;

use crate::Error;

const DISCONNECTED: i32 = -1;
const NO_ERROR: i32 = 0;
const QUARTER_TURN_DEGREES: i32 = 90;
const FULL_TURN_QUARTERS: i32 = 4;
/// AE target fps range is `[min, max]`.
const FPS_RANGE_LEN: usize = 2;
const SINGLE_REQUEST: c_int = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Facing {
    Front,
    Back,
}

impl Facing {
    const fn lens_facing(self) -> u32 {
        match self {
            Self::Front => ffi::acamera_metadata_enum_acamera_lens_facing::ACAMERA_LENS_FACING_FRONT.0,
            Self::Back => ffi::acamera_metadata_enum_acamera_lens_facing::ACAMERA_LENS_FACING_BACK.0,
        }
    }

    #[must_use]
    pub const fn flipped(self) -> Self {
        match self {
            Self::Front => Self::Back,
            Self::Back => Self::Front,
        }
    }
}

/// Last asynchronous device error, written from camera callbacks.
#[derive(Default)]
pub struct DeviceError(AtomicI32);

impl DeviceError {
    /// `None` when healthy; `Some(-1)` when disconnected, else `ERROR_CAMERA_*`.
    pub fn get(&self) -> Option<i32> {
        Some(self.0.load(Ordering::Relaxed)).filter(|&e| e != NO_ERROR)
    }
}

unsafe extern "C" fn on_disconnected(ctx: *mut c_void, _: *mut ffi::ACameraDevice) {
    // SAFETY: ctx is the `DeviceError` kept alive by the owning `Camera`.
    unsafe { &*ctx.cast::<DeviceError>() }.0.store(DISCONNECTED, Ordering::Relaxed);
}

unsafe extern "C" fn on_error(ctx: *mut c_void, _: *mut ffi::ACameraDevice, error: c_int) {
    // SAFETY: as in `on_disconnected`.
    unsafe { &*ctx.cast::<DeviceError>() }.0.store(error, Ordering::Relaxed);
}

fn check(call: &'static str, status: ffi::camera_status_t) -> Result<(), Error> {
    if status == ffi::camera_status_t::ACAMERA_OK { Ok(()) } else { Err(Error::Camera { call, status: status.0 }) }
}

pub struct Camera {
    mgr: *mut ffi::ACameraManager,
    device: *mut ffi::ACameraDevice,
    outputs: *mut ffi::ACaptureSessionOutputContainer,
    session_outputs: Vec<*mut ffi::ACaptureSessionOutput>,
    targets: Vec<*mut ffi::ACameraOutputTarget>,
    request: *mut ffi::ACaptureRequest,
    session: *mut ffi::ACameraCaptureSession,
    // Referenced by the device/session until they are closed in `Drop`.
    device_callbacks: Box<ffi::ACameraDevice_StateCallbacks>,
    session_callbacks: Box<ffi::ACameraCaptureSession_stateCallbacks>,
    error: Arc<DeviceError>,
    id: String,
    sensor_orientation: i32,
    facing: Facing,
    intent: Intent,
}

/// What a session is for. The request template sets `CONTROL_CAPTURE_INTENT`, which is how the
/// camera HAL learns that one of these streams feeds a video encoder rather than a viewfinder.
/// Getting it wrong is not cosmetic: a HiSilicon device handed an encoder surface under the
/// preview intent configured the session, delivered no frames at all, and then faulted with
/// `ERROR_CAMERA_DEVICE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    Preview,
    Record,
}

impl Camera {
    /// Opens the first camera facing `facing` and repeats a capture request into every window.
    pub fn open(facing: Facing, windows: &[&NativeWindow], fps: i32, intent: Intent) -> Result<Self, Error> {
        let error = Arc::<DeviceError>::default();
        let ctx = Arc::as_ptr(&error).cast_mut().cast::<c_void>();
        // SAFETY: plain C constructor.
        let mgr = unsafe { ffi::ACameraManager_create() };
        let mut cam = Self {
            mgr,
            device: null_mut(),
            outputs: null_mut(),
            session_outputs: Vec::with_capacity(windows.len()),
            targets: Vec::with_capacity(windows.len()),
            request: null_mut(),
            session: null_mut(),
            device_callbacks: Box::new(ffi::ACameraDevice_StateCallbacks {
                context: ctx,
                onDisconnected: Some(on_disconnected),
                onError: Some(on_error),
                onClientSharedAccessPriorityChanged: None,
            }),
            session_callbacks: Box::new(ffi::ACameraCaptureSession_stateCallbacks {
                context: null_mut(),
                onClosed: None,
                onReady: None,
                onActive: None,
            }),
            error,
            id: String::new(),
            sensor_orientation: 0,
            facing,
            intent,
        };
        let (id, orientation) = find(mgr, facing)?;
        cam.id = id.to_string_lossy().into_owned();
        cam.sensor_orientation = orientation;
        let range = [fps; FPS_RANGE_LEN];
        // SAFETY: every out-pointer is a field of `cam`; `Drop` releases whatever was created
        // if a later step fails.
        unsafe {
            check(
                "openCamera",
                ffi::ACameraManager_openCamera(mgr, id.as_ptr(), &raw mut *cam.device_callbacks, &raw mut cam.device),
            )?;
            check(
                "ACaptureSessionOutputContainer_create",
                ffi::ACaptureSessionOutputContainer_create(&raw mut cam.outputs),
            )?;
            for window in windows {
                let mut output = null_mut();
                check(
                    "ACaptureSessionOutput_create",
                    ffi::ACaptureSessionOutput_create(window.ptr().as_ptr(), &raw mut output),
                )?;
                cam.session_outputs.push(output);
                check(
                    "ACaptureSessionOutputContainer_add",
                    ffi::ACaptureSessionOutputContainer_add(cam.outputs, output),
                )?;
            }
            check(
                "createCaptureSession",
                ffi::ACameraDevice_createCaptureSession(
                    cam.device,
                    cam.outputs,
                    &raw const *cam.session_callbacks,
                    &raw mut cam.session,
                ),
            )?;
            check(
                "createCaptureRequest",
                ffi::ACameraDevice_createCaptureRequest(
                    cam.device,
                    match intent {
                        Intent::Preview => ffi::ACameraDevice_request_template::TEMPLATE_PREVIEW,
                        Intent::Record => ffi::ACameraDevice_request_template::TEMPLATE_RECORD,
                    },
                    &raw mut cam.request,
                ),
            )?;
            for window in windows {
                let mut target = null_mut();
                check(
                    "ACameraOutputTarget_create",
                    ffi::ACameraOutputTarget_create(window.ptr().as_ptr(), &raw mut target),
                )?;
                cam.targets.push(target);
                check("ACaptureRequest_addTarget", ffi::ACaptureRequest_addTarget(cam.request, target))?;
            }
            check(
                "AE_TARGET_FPS_RANGE",
                ffi::ACaptureRequest_setEntry_i32(
                    cam.request,
                    ffi::acamera_metadata_tag::ACAMERA_CONTROL_AE_TARGET_FPS_RANGE.0,
                    FPS_RANGE_LEN as u32,
                    range.as_ptr(),
                ),
            )?;
            check(
                "setRepeatingRequest",
                ffi::ACameraCaptureSession_setRepeatingRequest(
                    cam.session,
                    null_mut(),
                    SINGLE_REQUEST,
                    &raw mut cam.request,
                    null_mut(),
                ),
            )?;
        }
        Ok(cam)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn facing(&self) -> Facing {
        self.facing
    }

    pub const fn intent(&self) -> Intent {
        self.intent
    }

    pub const fn sensor_orientation(&self) -> i32 {
        self.sensor_orientation
    }

    /// Quarter turns that bring the sensor image upright (preview shader convention).
    pub const fn upright_quarter_turns(&self) -> i32 {
        self.sensor_orientation / QUARTER_TURN_DEGREES % FULL_TURN_QUARTERS
    }

    pub fn error(&self) -> Option<i32> {
        self.error.get()
    }
}

fn find(mgr: *mut ffi::ACameraManager, facing: Facing) -> Result<(CString, i32), Error> {
    let mut list = null_mut();
    // SAFETY: `mgr` is valid; `list` is freed below.
    check("getCameraIdList", unsafe { ffi::ACameraManager_getCameraIdList(mgr, &raw mut list) })?;
    // SAFETY: the list stays valid until deleteCameraIdList.
    let ids =
        unsafe { std::slice::from_raw_parts((*list).cameraIds, usize::try_from((*list).numCameras).unwrap_or(0)) };
    let found = ids.iter().find_map(|&id| {
        // SAFETY: ids are valid C strings owned by `list`.
        let (lens, orientation) = unsafe { characteristics(mgr, id) }?;
        // SAFETY: as above.
        (lens == facing.lens_facing()).then(|| (unsafe { CStr::from_ptr(id) }.to_owned(), orientation))
    });
    // SAFETY: `list` came from getCameraIdList.
    unsafe { ffi::ACameraManager_deleteCameraIdList(list) };
    found.ok_or(Error::NoCamera(facing))
}

/// (lens facing, sensor orientation degrees) of camera `id`.
unsafe fn characteristics(mgr: *mut ffi::ACameraManager, id: *const std::ffi::c_char) -> Option<(u32, i32)> {
    let mut meta = null_mut();
    // SAFETY: caller passes a valid manager and id.
    let status = unsafe { ffi::ACameraManager_getCameraCharacteristics(mgr, id, &raw mut meta) };
    if status != ffi::camera_status_t::ACAMERA_OK {
        return None;
    }
    let entry = |tag: ffi::acamera_metadata_tag| {
        // SAFETY: zeroed is a valid bit pattern for the C entry struct.
        let mut e: ffi::ACameraMetadata_const_entry = unsafe { std::mem::zeroed() };
        // SAFETY: `meta` is valid until freed below.
        let status = unsafe { ffi::ACameraMetadata_getConstEntry(meta, tag.0, &raw mut e) };
        (status == ffi::camera_status_t::ACAMERA_OK).then_some(e)
    };
    // SAFETY: LENS_FACING is a u8 entry and SENSOR_ORIENTATION an i32 entry, per the metadata spec.
    let lens = entry(ffi::acamera_metadata_tag::ACAMERA_LENS_FACING).map(|e| u32::from(unsafe { *e.data.u8_ }));
    let orientation = entry(ffi::acamera_metadata_tag::ACAMERA_SENSOR_ORIENTATION).map(|e| unsafe { *e.data.i32_ });
    // SAFETY: `meta` came from getCameraCharacteristics.
    unsafe { ffi::ACameraMetadata_free(meta) };
    Some((lens?, orientation.unwrap_or(0)))
}

impl Drop for Camera {
    fn drop(&mut self) {
        // SAFETY: each handle is either null or was created by `open` and is released once.
        unsafe {
            if !self.session.is_null() {
                ffi::ACameraCaptureSession_stopRepeating(self.session);
                ffi::ACameraCaptureSession_close(self.session);
            }
            if !self.request.is_null() {
                ffi::ACaptureRequest_free(self.request);
            }
            for &target in &self.targets {
                ffi::ACameraOutputTarget_free(target);
            }
            if !self.device.is_null() {
                ffi::ACameraDevice_close(self.device);
            }
            if !self.outputs.is_null() {
                ffi::ACaptureSessionOutputContainer_free(self.outputs);
            }
            for &output in &self.session_outputs {
                ffi::ACaptureSessionOutput_free(output);
            }
            ffi::ACameraManager_delete(self.mgr);
        }
    }
}
