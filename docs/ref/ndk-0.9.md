# ndk 0.9 / ndk-sys 0.6 (r30) / ndk-gl-sys — API reference

Forked and patched, see `ndk-vendoring.md`. Sources: `external/ndk/ndk/src/`, `external/ndk/ndk-sys/src/ffi_aarch64.rs`,
`sys/ndk-gl-sys/src/ffi_aarch64.rs`. Bindings are **generated from NDK r30**. Never hardcode NDK/GL constants;
look them up in the generated files. ✅ = verified by compiling or running in a spike.

## Crates and features

```toml
ndk = { version = "0.9", default-features = false, features = ["media", "api-level-26"] }
ndk-sys = { version = "0.6", features = ["camera", "media"] }   # `camera` links camera2ndk (our addition)
ndk-gl-sys = { path = "sys/ndk-gl-sys" }                         # EGL + GLES3 + extensions (ours, edition 2024)
```
- `ndk` features: `media` (ImageReader, MediaCodec), `nativewindow`, `audio`, `bitmap`, `sync`, and
  `api-level-23 … api-level-33` (cumulative). Some modules are gated with inner attributes,
  e.g. `data_space.rs` has `#![cfg(feature = "api-level-28")]`.
- `ndk-sys` link features: `media` → mediandk, `camera` → camera2ndk, `nativewindow`, `audio` → aaudio,
  `bitmap` → jnigraphics, `sync`. libandroid is always linked.
- `ndk-gl-sys` always links `libEGL` + `libGLESv3`. On Android those export the extension entry points we use, so
  there's no `eglGetProcAddress` dance: `eglGetNativeClientBufferANDROID`, `eglCreateImageKHR`, `eglDestroyImageKHR`
  (libEGL), and `glEGLImageTargetTexture2DOES`, `glBindVertexArray`, `glBindSampler` (libGLESv3). ✅ checked the stubs' dynsym.

## Binding conventions (bindgen, upstream flags)

- **Newtype enums** (`--newtype-enum`): `camera_status_t(pub c_int)`, `acamera_metadata_tag(pub u32)`,
  `ACameraDevice_request_template(pub u32)`, `AIMAGE_FORMATS`, `AHardwareBuffer_UsageFlags`, `ADataSpace`, … Values are
  associated consts: `ffi::acamera_metadata_tag::ACAMERA_LENS_FACING`, `ffi::camera_status_t::ACAMERA_OK`.
  Use `.0` for the raw value (e.g. `ACaptureRequest_setEntry_i32(req, tag.0, …)`).
- Enums matching `acamera_\w+` include metadata enum values, e.g.
  `ffi::acamera_metadata_enum_acamera_lens_facing::ACAMERA_LENS_FACING_FRONT` (a u32 newtype; the metadata stores a u8).
- Plain `#define` ints become `pub const NAME: u32`. `ndk-gl-sys` constants follow this: `GL_TEXTURE_2D: u32`,
  `EGL_NATIVE_BUFFER_ANDROID: u32`. Cast to `GLenum`/`EGLint` as needed.
- Casted macros (`EGL_NO_CONTEXT`, `EGL_NO_IMAGE_KHR`, …) are **not** generated; use `ptr::null_mut()`.

## Safe `ndk` wrappers used

### ImageReader (`ndk::media::image_reader`, feature `media`)
```rust
use ndk::media::image_reader::{ImageReader, ImageFormat, AcquireResult};
use ndk::hardware_buffer::HardwareBufferUsage;
let mut r = ImageReader::new_with_usage(w, h, ImageFormat::PRIVATE,          // api-level-26
            HardwareBufferUsage::GPU_SAMPLED_IMAGE, max_images)?;
r.set_image_listener(Box::new(|reader: &ImageReader| { /* camera thread */ }))?;  // Box<dyn FnMut(&ImageReader)+Send>
let win: ndk::native_window::NativeWindow = r.window()?;   // win.ptr(): NonNull<ffi::ANativeWindow>
match r.acquire_latest_image()? {                           // also acquire_next_image
    AcquireResult::Image(img) => { let hb = img.hardware_buffer()?; /* hb.as_ptr(): *mut ffi::AHardwareBuffer */ }
    AcquireResult::NoBufferAvailable | AcquireResult::MaxImagesAcquired => {}
}
```
- `Image` accessors: `plane_data(i)`, `plane_pixel_stride(i)`, `plane_row_stride(i)`, `crop_rect()`, `width()`, `height()`,
  `format()`, `timestamp()`, `number_of_planes()`, `hardware_buffer()` (api-26), `delete_async(fence)`.
- A `HardwareBuffer` from `hardware_buffer()` is valid only while the `Image` lives. `HardwareBuffer::acquire()` gives an
  owned reference (then also register `set_buffer_removed_listener`).
- `ImageFormat::PRIVATE` + `GPU_SAMPLED_IMAGE` = the camera's opaque native format, which the GPU can sample (zero-copy).
  Use `YUV_420_888` for CPU access.

## Camera2 (raw `ndk_sys`, feature `camera`)

No safe wrapper in `ndk`. The call sequence, with signatures from the r30 bindings:
```text
ACameraManager_create() -> *mut ACameraManager
ACameraManager_getCameraIdList(mgr, &mut *mut ACameraIdList)        // { numCameras: c_int, cameraIds: *mut *const c_char }
ACameraManager_getCameraCharacteristics(mgr, id, &mut *mut ACameraMetadata)
ACameraMetadata_getConstEntry(meta, tag: u32, &mut ACameraMetadata_const_entry)
    // entry { tag: u32, type_: u8, count: u32, data: union { u8_, i32_, f, i64_, d, r } }
ACameraManager_openCamera(mgr, id, *mut ACameraDevice_StateCallbacks, &mut *mut ACameraDevice)
ACaptureSessionOutputContainer_create(&mut c); ACaptureSessionOutput_create(anw, &mut o); ACaptureSessionOutputContainer_add(c, o)
ACameraDevice_createCaptureSession(dev, c, *const ACameraCaptureSession_stateCallbacks, &mut session)
ACameraDevice_createCaptureRequest(dev, ACameraDevice_request_template::TEMPLATE_PREVIEW, &mut req)
ACameraOutputTarget_create(anw, &mut t); ACaptureRequest_addTarget(req, t)
ACaptureRequest_setEntry_i32(req, tag.0, count, *const i32)          // e.g. ACAMERA_CONTROL_AE_TARGET_FPS_RANGE [30,30]
ACameraCaptureSession_setRepeatingRequest(session, null_mut(), 1, &mut req, null_mut())
teardown: stopRepeating → ACameraCaptureSession_close → ACameraDevice_close → free request/target/outputs → ACameraManager_delete
```
- **r30 change:** `ACameraDevice_StateCallbacks` has **4** fields:
  `{ context, onDisconnected, onError, onClientSharedAccessPriorityChanged }`. Set the new one to `None`.
- Callback types: `ACameraDevice_StateCallback = Option<unsafe extern "C" fn(ctx, *mut ACameraDevice)>`,
  `ACameraDevice_ErrorStateCallback = Option<… fn(ctx, dev, error: c_int)>`,
  `ACameraCaptureSession_stateCallback = Option<… fn(ctx, *mut ACameraCaptureSession)>` (onClosed/onReady/onActive).
- `ACameraWindowType = ANativeWindow`.
- Useful tags: `ACAMERA_LENS_FACING` (u8: FRONT=0, BACK=1, EXTERNAL=2), `ACAMERA_SENSOR_ORIENTATION` (i32 degrees),
  `ACAMERA_CONTROL_AE_TARGET_FPS_RANGE` (i32[2]), `ACAMERA_SCALER_AVAILABLE_STREAM_CONFIGURATIONS` (i32[n*4]).
- The runtime CAMERA permission is needed first (see `jni-0.22.md`: `checkSelfPermission` / `requestPermissions` via JNI).

## New in r30 vs upstream bindings (relevant)

`AImage_getTransform`, `AImageReader_setDefaultBufferSize`, `AImageReader_setDefaultAHardwareBufferFormat`,
`AImageReader_setDefaultBufferDataSpace`, `ACameraManager_openSharedCamera` + `ACameraCaptureSessionShared_*`,
`ACameraMetadata_getTagFromName`, `ANativeWindow_setProducerThrottlingEnabled`.

## Verified on device (spike 3, S24, 2026-09-22) ✅

- Front camera: `ACAMERA_LENS_FACING_FRONT`, 1280x720 `PRIVATE` + `GPU_SAMPLED_IMAGE`, `max_images = 4`, repeating
  `TEMPLATE_PREVIEW` with AE fps [30,30] gives a steady 30 fps.
- `AImage::hardware_buffer()` → `eglGetNativeClientBufferANDROID` → `eglCreateImageKHR(EGL_NATIVE_BUFFER_ANDROID)`
  → `glEGLImageTargetTexture2DOES(GL_TEXTURE_EXTERNAL_OES)` works on Skia's GL context. The EGLImage is created and
  destroyed per frame; keep the `Image` alive until the next frame replaces it.
- Orientation: turn by **sensor_orientation / 90** quarter turns; front cameras are mirrored in **display space
  before** rotating (mirroring after rotation reverses the turn direction and broke the 270° front camera).
  The front camera needed no extra mirror beyond the default `mirror = front`.
- Start → stop → start and pause/resume teardown order work: drop the shown `Image`, then the camera session, then the `ImageReader`.
