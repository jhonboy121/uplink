package dev.uplink;

import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;
import android.security.keystore.StrongBoxUnavailableException;
import android.util.Log;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.security.GeneralSecurityException;
import java.security.Key;
import java.security.KeyStore;
import java.security.KeyStoreException;
import java.util.Arrays;
import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.spec.GCMParameterSpec;

/**
 * The device identity at rest: sealed with an AES-256-GCM key that never leaves the Android
 * Keystore (StrongBox where the phone has one), so the file alone is not the identity. No user
 * authentication, since the app starts headless at boot and after updates. A sealed identity the
 * Keystore has no key for (a restore onto another phone) fails to open, and says so, rather than
 * the device quietly becoming someone new.
 */
final class UplinkKeystore {
  private static final String PROVIDER = "AndroidKeyStore";
  private static final String ALIAS = "uplink-identity";
  private static final String TRANSFORMATION = "AES/GCM/NoPadding";
  private static final int KEY_BITS = 256;
  private static final int TAG_BITS = 128;
  private static final int IV_BYTES = 12;

  /** The first byte of a sealed identity: this layout, the IV and then ciphertext and tag. */
  private static final byte FORMAT = 1;

  private static final int HEADER = 1 + IV_BYTES;

  private UplinkKeystore() {}

  /** Seals {@code secret}, which is zeroed after, making the Keystore key on first use. */
  static byte[] seal(byte[] secret) throws GeneralSecurityException, IOException {
    try {
      Cipher cipher = Cipher.getInstance(TRANSFORMATION);
      // The Keystore picks the IV: a key it holds refuses one we choose.
      cipher.init(Cipher.ENCRYPT_MODE, key(true));
      byte[] iv = cipher.getIV();
      if (iv.length != IV_BYTES) {
        throw new GeneralSecurityException("unexpected IV length " + iv.length);
      }
      byte[] sealed = cipher.doFinal(secret);
      return ByteBuffer.allocate(HEADER + sealed.length).put(FORMAT).put(iv).put(sealed).array();
    } finally {
      Arrays.fill(secret, (byte) 0);
    }
  }

  static byte[] open(byte[] sealed) throws GeneralSecurityException, IOException {
    if (sealed.length <= HEADER || sealed[0] != FORMAT) {
      throw new GeneralSecurityException("not a sealed identity");
    }
    Cipher cipher = Cipher.getInstance(TRANSFORMATION);
    cipher.init(
        Cipher.DECRYPT_MODE, key(false), new GCMParameterSpec(TAG_BITS, sealed, 1, IV_BYTES));
    return cipher.doFinal(sealed, HEADER, sealed.length - HEADER);
  }

  private static SecretKey key(boolean create) throws GeneralSecurityException, IOException {
    KeyStore store = KeyStore.getInstance(PROVIDER);
    store.load(null);
    Key existing = store.getKey(ALIAS, null);
    if (existing instanceof SecretKey) {
      return (SecretKey) existing;
    }
    if (!create) {
      throw new KeyStoreException("the Keystore has no identity key for this sealed identity");
    }
    try {
      return generate(true);
    } catch (StrongBoxUnavailableException e) {
      UplinkApplication.log(Log.INFO, "keystore: no StrongBox, identity key in the TEE");
      return generate(false);
    }
  }

  private static SecretKey generate(boolean strongBox) throws GeneralSecurityException {
    KeyGenerator generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, PROVIDER);
    generator.init(
        new KeyGenParameterSpec.Builder(
                ALIAS, KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(KEY_BITS)
            .setIsStrongBoxBacked(strongBox)
            .build());
    SecretKey key = generator.generateKey();
    UplinkApplication.log(Log.INFO, "keystore: identity key made, StrongBox " + strongBox);
    return key;
  }
}
