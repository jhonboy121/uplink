package dev.uplink;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.content.Context;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.OpenableColumns;

import java.io.File;
import java.io.FileNotFoundException;

/**
 * Hands one directory of the app's cache to whatever the user shares to. A file:// URI is refused
 * by the system at this target level, so a share needs a content:// one, and that needs a
 * provider — this is the smallest one that works, rather than a dependency on androidx.
 *
 * <p>It is not exported: nothing can read through it except the app the user picked from the
 * chooser, and only the URI that was granted to it.
 */
public class UplinkFiles extends ContentProvider {
    /** Inside the app's files directory, which is what Rust knows as its data directory. */
    static final String DIRECTORY = "share";
    private static final String PNG = "image/png";
    private static final String TEXT = "text/plain";

    /** The type a name implies. Only what the app actually shares is worth listing. */
    static String typeOf(String name) {
        return name != null && name.endsWith(".png") ? PNG : TEXT;
    }

    /** The URI for a file already written into {@link #DIRECTORY}. */
    static Uri uriFor(Context context, String name) throws FileNotFoundException {
        if (!new File(directory(context), name).isFile()) {
            throw new FileNotFoundException("nothing to share at " + DIRECTORY + "/" + name);
        }
        return new Uri.Builder()
                .scheme("content")
                .authority(context.getPackageName() + ".files")
                .appendPath(name)
                .build();
    }

    /** Where Rust writes what is meant to leave the app. Nothing else belongs here. */
    private static File directory(Context context) {
        return new File(context.getFilesDir(), DIRECTORY);
    }

    @Override
    public boolean onCreate() {
        return true;
    }

    @Override
    public ParcelFileDescriptor openFile(Uri uri, String mode) throws FileNotFoundException {
        return ParcelFileDescriptor.open(resolve(uri), ParcelFileDescriptor.MODE_READ_ONLY);
    }

    @Override
    public String getType(Uri uri) {
        return typeOf(uri.getLastPathSegment());
    }

    /** Share targets ask for the name and size before they read anything. */
    @Override
    public Cursor query(Uri uri, String[] projection, String selection, String[] args, String order) {
        File file;
        try {
            file = resolve(uri);
        } catch (FileNotFoundException e) {
            return null;
        }
        String[] columns = projection != null
                ? projection
                : new String[] {OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE};
        MatrixCursor cursor = new MatrixCursor(columns, 1);
        MatrixCursor.RowBuilder row = cursor.newRow();
        for (String column : columns) {
            if (OpenableColumns.DISPLAY_NAME.equals(column)) {
                row.add(file.getName());
            } else if (OpenableColumns.SIZE.equals(column)) {
                row.add(file.length());
            } else {
                row.add(null);
            }
        }
        return cursor;
    }

    /** One name, in one directory: a path that is anything else is not ours to serve. */
    private File resolve(Uri uri) throws FileNotFoundException {
        Context context = getContext();
        String name = uri.getLastPathSegment();
        if (context == null || name == null || uri.getPathSegments().size() != 1
                || name.contains(File.separator) || name.equals("..")) {
            throw new FileNotFoundException("not a shared file: " + uri);
        }
        File file = new File(directory(context), name);
        if (!file.isFile()) {
            throw new FileNotFoundException("no such shared file: " + name);
        }
        return file;
    }

    @Override
    public Uri insert(Uri uri, ContentValues values) {
        throw new UnsupportedOperationException("read-only");
    }

    @Override
    public int update(Uri uri, ContentValues values, String selection, String[] args) {
        throw new UnsupportedOperationException("read-only");
    }

    @Override
    public int delete(Uri uri, String selection, String[] args) {
        throw new UnsupportedOperationException("read-only");
    }
}
