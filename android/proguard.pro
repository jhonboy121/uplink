# R8's rules for the Java shim in a release build (`just profile=release apk`).
#
# Two ways in find our code by name, and anything R8 renamed or dropped there would fail at run
# time, not at build time:
#   - Rust, over JNI: it looks up UplinkApplication by name, registers its native methods on it,
#     and calls methods on it and on the activity by name. All of both stays as written.
#   - Android, from the manifest: the application, activity, services, receiver and provider are
#     made by reflection. Their names and constructors stay; framework overrides are kept anyway.
# Everything else (Telecom's glue, the Keystore, the connection) may be shrunk, inlined, renamed.

-keep class dev.uplink.UplinkApplication { *; }
-keep class dev.uplink.UplinkActivity { *; }

-keep class dev.uplink.UplinkFiles { <init>(); }
-keep class dev.uplink.UplinkListenService { <init>(); }
-keep class dev.uplink.UplinkConnectionService { <init>(); }
-keep class dev.uplink.UplinkBootReceiver { <init>(); }
-keep class dev.uplink.UplinkCallService { <init>(); }

# A Java stack trace in the log stays readable with the mapping `just apk` writes beside the APK.
-keepattributes SourceFile,LineNumberTable
-renamesourcefileattribute SourceFile
