package com.shadowgraph.daemon

import com.shadowgraph.daemon.config.DaemonConfig
import com.shadowgraph.daemon.transport.ByteArrayBuffer
import com.shadowgraph.daemon.transport.CommandParser
import com.shadowgraph.daemon.transport.InboundCommand
import com.shadowgraph.daemon.transport.NdjsonWireEncoder
import com.shadowgraph.daemon.transport.WireVisionFrame
import com.shadowgraph.daemon.vision.VisionCapturer
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue

class VisionCapturerTest {

    @Test
    fun `checkSecurityGuard strictly blocks on FLAG_SECURE and permits non-secure screens`() {
        val blocked = VisionCapturer.checkSecurityGuard(isSecureScreenActive = true, quality = 50)
        assertTrue(blocked != null)
        assertFalse(blocked.ok)
        assertEquals("FLAG_SECURE", blocked.blockedReason)
        assertEquals("vision_blocked_flag_secure", blocked.error)
        assertNull(blocked.dataBase64)
        assertEquals(0, blocked.byteLength)

        assertNull(VisionCapturer.checkSecurityGuard(isSecureScreenActive = false, quality = 50))
    }

    @Test
    fun `computeTargetDimensions downscales proportionally and never upscales`() {
        assertEquals(540 to 1200, VisionCapturer.computeTargetDimensions(1080, 2400, 540))
        assertEquals(400 to 800, VisionCapturer.computeTargetDimensions(400, 800, 540))
        assertEquals(160 to 320, VisionCapturer.computeTargetDimensions(1000, 2000, 50)) // clamped to 160
        assertEquals(0 to 0, VisionCapturer.computeTargetDimensions(0, 100, 540))
    }

    @Test
    fun `CommandParser parses capture_vision with defaults and clamping`() {
        val def = CommandParser.parseLine("""{"cmd":"capture_vision","id":7}""")
        val cmdDef = assertIs<InboundCommand.CaptureVision>(def)
        assertEquals(7L, cmdDef.id)
        assertEquals(VisionCapturer.DEFAULT_MAX_WIDTH, cmdDef.maxWidth)
        assertEquals(VisionCapturer.DEFAULT_QUALITY, cmdDef.quality)

        val custom = CommandParser.parseLine("""{"cmd":"capture_vision","id":9,"max_width":9999,"quality":5}""")
        val cmdCustom = assertIs<InboundCommand.CaptureVision>(custom)
        assertEquals(9L, cmdCustom.id)
        assertEquals(VisionCapturer.MAX_MAX_WIDTH, cmdCustom.maxWidth)
        assertEquals(VisionCapturer.MIN_QUALITY, cmdCustom.quality)
    }

    @Test
    fun `NdjsonWireEncoder encodes ok and FLAG_SECURE vision_frame payloads`() {
        val encoder = NdjsonWireEncoder()
        val buf = ByteArrayBuffer(512)

        encoder.encodeVisionFrame(
            WireVisionFrame(
                id = 42L,
                ok = true,
                mimeType = "image/jpeg",
                width = 540,
                height = 1170,
                quality = 45,
                byteLength = 1234,
                dataBase64 = "/9j/4AAQ",
                elapsedMs = 18L,
                timestampMs = 1700000000000L
            ),
            buf
        )
        val okJson = buf.toByteArray().decodeToString()
        assertTrue(okJson.contains(""""type":"vision_frame""""))
        assertTrue(okJson.contains(""""id":42"""))
        assertTrue(okJson.contains(""""ok":true"""))
        assertTrue(okJson.contains(""""data_base64":"/9j/4AAQ""""))
        assertTrue(okJson.endsWith("\n"))

        encoder.encodeVisionFrame(
            WireVisionFrame(
                id = 43L,
                ok = false,
                blockedReason = "FLAG_SECURE",
                error = "vision_blocked_flag_secure",
                timestampMs = 1700000000001L
            ),
            buf
        )
        val blockedJson = buf.toByteArray().decodeToString()
        assertTrue(blockedJson.contains(""""ok":false"""))
        assertTrue(blockedJson.contains(""""blocked_reason":"FLAG_SECURE""""))
        assertFalse(blockedJson.contains("data_base64"))
    }

    @Test
    fun `DaemonConfig supports ephemeral port 0 and headless boot flags`() {
        val cfg = DaemonConfig.parse(
            arrayOf("--port=0", "--wait-boot", "--connect-retries=15", "--connect-retry-delay-ms=250")
        )
        assertEquals(0, cfg.port)
        assertTrue(cfg.waitBoot)
        assertEquals(15, cfg.connectRetries)
        assertEquals(250L, cfg.connectRetryDelayMs)
    }
}
