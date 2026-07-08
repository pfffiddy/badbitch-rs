package dev.llmdesk.app

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.provider.OpenableColumns
import com.google.androidgamesdk.GameActivity
import java.io.File

/**
 * Thin shell: GameActivity hosts the native surface; rendering, input, and
 * networking are the shared Rust/egui code — the very same crate the desktop
 * window uses.
 *
 * The one Android-specific bit is the model-file picker: the native UI asks us
 * (via [pickModelFile]) to open the system document picker, we copy the chosen
 * file somewhere the native code can read, and hand back the path through
 * [nativeOnFilePicked], which streams it to the desktop.
 */
class MainActivity : GameActivity() {
    companion object {
        init {
            // Belt-and-braces alongside the manifest meta-data.
            System.loadLibrary("llm_desk_android")
        }

        private const val PICK_MODEL_REQUEST = 4832
    }

    /** Implemented in Rust; receives the on-disk path of the picked file. */
    private external fun nativeOnFilePicked(path: String)

    /** Called from Rust to launch the system document picker. */
    @Suppress("unused")
    fun pickModelFile() {
        runOnUiThread {
            val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                addCategory(Intent.CATEGORY_OPENABLE)
                type = "*/*"
            }
            try {
                startActivityForResult(intent, PICK_MODEL_REQUEST)
            } catch (_: Exception) {
                // No document-picker activity available; nothing to do.
            }
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != PICK_MODEL_REQUEST || resultCode != Activity.RESULT_OK) return
        val uri = data?.data ?: return

        // Copy off the UI thread — model files can be large.
        Thread {
            try {
                val name = displayName(uri) ?: "model.gguf"
                val dest = File(cacheDir, safeName(name))
                contentResolver.openInputStream(uri)?.use { input ->
                    dest.outputStream().use { output -> input.copyTo(output, 1 shl 20) }
                }
                nativeOnFilePicked(dest.absolutePath)
            } catch (_: Exception) {
                // Best-effort; a failed copy just means no upload happens.
            }
        }.start()
    }

    /** Keep only safe characters and force a .gguf suffix. */
    private fun safeName(raw: String): String {
        val base = raw.substringAfterLast('/')
        val cleaned = base.map {
            if (it.isLetterOrDigit() || it == '-' || it == '_' || it == '.') it else '-'
        }.joinToString("")
        return if (cleaned.endsWith(".gguf")) cleaned else "$cleaned.gguf"
    }

    private fun displayName(uri: Uri): String? {
        contentResolver.query(uri, null, null, null, null)?.use { cursor ->
            val idx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
            if (idx >= 0 && cursor.moveToFirst()) return cursor.getString(idx)
        }
        return null
    }
}
