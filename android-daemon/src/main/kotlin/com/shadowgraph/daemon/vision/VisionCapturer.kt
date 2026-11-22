package com.shadowgraph.daemon.vision

import android.app.UiAutomation
import android.graphics.Bitmap
import android.os.SystemClock
import java.io.ByteArrayOutputStream
import java.util.Base64
import kotlin.math.max
import kotlin.math.roundToInt

data class VisionCaptureResult(
    val ok: Boolean,
    val mimeType: String = "image/jpeg",
    val width: Int = 0,
    val height: Int = 0,
    val quality: Int = VisionCapturer.DEFAULT_QUALITY,
    val byteLength: Int = 0,
    val dataBase64: String? = null,
    val blockedReason: String? = null,
    val error: String? = null,
    val elapsedMs: Long = 0L
)

/**
 * Opt-in, heavily compressed JPEG screenshot capturer for hybrid UI Tree + Vision streaming.
 *
 * Security invariant: MUST NEVER capture or emit pixels when [isSecureScreenActive] is true.
 */
object VisionCapturer {
    const val MIN_MAX_WIDTH = 160
    const val MAX_MAX_WIDTH = 1080
    const val DEFAULT_MAX_WIDTH = 540

    const val MIN_QUALITY = 10
    const val MAX_QUALITY = 85
    const val DEFAULT_QUALITY = 50

    fun clampMaxWidth(raw: Int): Int = raw.coerceIn(MIN_MAX_WIDTH, MAX_MAX_WIDTH)

    fun clampQuality(raw: Int): Int = raw.coerceIn(MIN_QUALITY, MAX_QUALITY)

    /**
     * Computes proportionally downscaled `(width, height)` so that `width <= maxWidth`,
     * never upscaling smaller frames.
     */
    fun computeTargetDimensions(srcWidth: Int, srcHeight: Int, maxWidth: Int): Pair<Int, Int> {
        if (srcWidth <= 0 || srcHeight <= 0) return 0 to 0
        val clampedMax = clampMaxWidth(maxWidth)
        if (srcWidth <= clampedMax) return srcWidth to srcHeight
        val scale = clampedMax.toDouble() / srcWidth.toDouble()
        val dstWidth = max(1, clampedMax)
        val dstHeight = max(1, (srcHeight.toDouble() * scale).roundToInt())
        return dstWidth to dstHeight
    }

    /**
     * Pure JVM-testable privacy & parameter guard checked before any `UiAutomation.takeScreenshot()` call.
     * Returns a non-null blocked [VisionCaptureResult] if capture must be refused.
     */
    fun checkSecurityGuard(
        isSecureScreenActive: Boolean,
        quality: Int = DEFAULT_QUALITY
    ): VisionCaptureResult? {
        if (!isSecureScreenActive) return null
        return VisionCaptureResult(
            ok = false,
            quality = clampQuality(quality),
            blockedReason = "FLAG_SECURE",
            error = "vision_blocked_flag_secure",
            elapsedMs = 0L
        )
    }

    fun capture(
        uia: UiAutomation?,
        isSecureScreenActive: Boolean,
        maxWidth: Int = DEFAULT_MAX_WIDTH,
        quality: Int = DEFAULT_QUALITY
    ): VisionCaptureResult {
        val clampedMaxWidth = clampMaxWidth(maxWidth)
        val clampedQuality = clampQuality(quality)

        checkSecurityGuard(isSecureScreenActive, clampedQuality)?.let { return it }

        if (uia == null) {
            return VisionCaptureResult(
                ok = false,
                quality = clampedQuality,
                error = "uiautomation_unavailable",
                elapsedMs = 0L
            )
        }

        val startMs = SystemClock.uptimeMillis()
        var rawBitmap: Bitmap? = null
        var scaledBitmap: Bitmap? = null
        return try {
            rawBitmap = uia.takeScreenshot()
            if (rawBitmap == null) {
                // Android SurfaceFlinger returns null when a FLAG_SECURE surface is active or display is off.
                return VisionCaptureResult(
                    ok = false,
                    quality = clampedQuality,
                    blockedReason = "FLAG_SECURE_OR_UNAVAILABLE",
                    error = "screenshot_blocked_or_unavailable",
                    elapsedMs = max(0L, SystemClock.uptimeMillis() - startMs)
                )
            }

            val (dstW, dstH) = computeTargetDimensions(rawBitmap.width, rawBitmap.height, clampedMaxWidth)
            val encodeBitmap = if (dstW > 0 && dstH > 0 && (dstW != rawBitmap.width || dstH != rawBitmap.height)) {
                scaledBitmap = Bitmap.createScaledBitmap(rawBitmap, dstW, dstH, true)
                scaledBitmap
            } else {
                rawBitmap
            }

            val baos = ByteArrayOutputStream(24 * 1024)
            val compressed = encodeBitmap.compress(Bitmap.CompressFormat.JPEG, clampedQuality, baos)
            if (!compressed) {
                return VisionCaptureResult(
                    ok = false,
                    quality = clampedQuality,
                    error = "jpeg_compress_failed",
                    elapsedMs = max(0L, SystemClock.uptimeMillis() - startMs)
                )
            }

            val jpegBytes = baos.toByteArray()
            val b64 = Base64.getEncoder().encodeToString(jpegBytes)
            VisionCaptureResult(
                ok = true,
                mimeType = "image/jpeg",
                width = encodeBitmap.width,
                height = encodeBitmap.height,
                quality = clampedQuality,
                byteLength = jpegBytes.size,
                dataBase64 = b64,
                elapsedMs = max(0L, SystemClock.uptimeMillis() - startMs)
            )
        } catch (t: Throwable) {
            VisionCaptureResult(
                ok = false,
                quality = clampedQuality,
                error = "${t.javaClass.simpleName}: ${t.message ?: "capture_failed"}",
                elapsedMs = max(0L, SystemClock.uptimeMillis() - startMs)
            )
        } finally {
            try {
                if (scaledBitmap != null && scaledBitmap !== rawBitmap && !scaledBitmap.isRecycled) {
                    scaledBitmap.recycle()
                }
            } catch (_: Throwable) {
            }
            try {
                if (rawBitmap != null && !rawBitmap.isRecycled) {
                    rawBitmap.recycle()
                }
            } catch (_: Throwable) {
            }
        }
    }
}
