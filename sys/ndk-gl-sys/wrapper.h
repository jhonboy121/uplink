// EGL + GLES3 with extension prototypes. On Android, libEGL/libGLESv3 export the
// extension entry points we use (eglGetNativeClientBufferANDROID, eglCreateImageKHR,
// glEGLImageTargetTexture2DOES), so they are linked directly.
#define EGL_EGLEXT_PROTOTYPES
#define GL_GLEXT_PROTOTYPES
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
