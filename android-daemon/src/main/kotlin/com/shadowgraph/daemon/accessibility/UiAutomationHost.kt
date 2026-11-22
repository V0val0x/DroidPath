package com.shadowgraph.daemon.accessibility

import android.accessibilityservice.AccessibilityServiceInfo
import android.app.UiAutomation
import android.os.HandlerThread
import android.os.Looper
import android.view.accessibility.AccessibilityEvent
import com.shadowgraph.daemon.compat.HiddenApiCompat
import com.shadowgraph.daemon.config.DaemonConfig
import java.lang.reflect.InvocationTargetException
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Owns the [HandlerThread] Looper and persistent [UiAutomation] connection,
 * adapting cleanly across Android 10–17 (SDK 29–37) hidden API variations and
 * guaranteeing idempotent `disconnect()` on teardown.
 */
class UiAutomationHost(private val config: DaemonConfig) {
    private val looperThread = HandlerThread("shadow-uia-looper")
    private val disconnected = AtomicBoolean(false)

    @Volatile
    var uiAutomation: UiAutomation? = null
        private set

    fun connect(): UiAutomation {
        HiddenApiCompat.exemptHiddenApis()

        if (config.waitBoot) {
            waitForBootCompleted()
        }

        if (!looperThread.isAlive) {
            looperThread.start()
        }
        val looper: Looper = looperThread.looper

        val maxAttempts = 1 + config.connectRetries.coerceAtLeast(0)
        var delayMs = config.connectRetryDelayMs.coerceAtLeast(50L)
        var lastError: Throwable? = null

        for (attempt in 1..maxAttempts) {
            var candidate: UiAutomation? = null
            try {
                val connectionClass = Class.forName("android.app.UiAutomationConnection")
                val iConnectionClass = Class.forName("android.app.IUiAutomationConnection")
                val connectionCtor = connectionClass.getDeclaredConstructor()
                connectionCtor.isAccessible = true
                val connection = connectionCtor.newInstance()

                val uiaCtor = UiAutomation::class.java.getDeclaredConstructor(
                    Looper::class.java,
                    iConnectionClass
                )
                uiaCtor.isAccessible = true
                val uia = uiaCtor.newInstance(looper, connection)
                candidate = uia

                invokeConnect(uia)
                configureServiceInfo(uia)

                uiAutomation = uia
                return uia
            } catch (t: Throwable) {
                lastError = t
                if (candidate != null) {
                    try {
                        val disconnectMethod = UiAutomation::class.java.getDeclaredMethod("disconnect")
                        disconnectMethod.isAccessible = true
                        disconnectMethod.invoke(candidate)
                    } catch (_: Throwable) {
                    }
                }
                if (attempt < maxAttempts) {
                    if (config.verbose) {
                        System.err.println(
                            "[ShadowGraph] UiAutomation connect attempt $attempt/$maxAttempts failed " +
                                "(${t.javaClass.simpleName}: ${t.message}); retrying in ${delayMs}ms"
                        )
                    }
                    try {
                        Thread.sleep(delayMs)
                    } catch (_: InterruptedException) {
                        Thread.currentThread().interrupt()
                        break
                    }
                    delayMs = (delayMs * 2L).coerceAtMost(3000L)
                }
            }
        }

        throw lastError ?: IllegalStateException("UiAutomation connect failed after $maxAttempts attempts")
    }

    private fun waitForBootCompleted() {
        val maxPolls = (config.connectRetries.coerceAtLeast(1) * 4).coerceAtLeast(12)
        val pollDelayMs = config.connectRetryDelayMs.coerceIn(100L, 1000L)
        for (i in 0 until maxPolls) {
            if (readSystemProperty("sys.boot_completed") == "1") {
                return
            }
            if (config.verbose && i == 0) {
                System.err.println("[ShadowGraph] Waiting for sys.boot_completed=1 (headless/emulator boot)...")
            }
            try {
                Thread.sleep(pollDelayMs)
            } catch (_: InterruptedException) {
                Thread.currentThread().interrupt()
                return
            }
        }
    }

    private fun readSystemProperty(key: String): String? {
        return try {
            val sp = Class.forName("android.os.SystemProperties")
            val getMethod = sp.getDeclaredMethod("get", String::class.java)
            getMethod.isAccessible = true
            getMethod.invoke(null, key) as? String
        } catch (_: Throwable) {
            null
        }
    }

    private fun invokeConnect(uia: UiAutomation) {
        // Android 10-17: try connect() first, then connect(int flags) if needed.
        try {
            val connectNoArg = UiAutomation::class.java.getDeclaredMethod("connect")
            connectNoArg.isAccessible = true
            connectNoArg.invoke(uia)
            return
        } catch (_: NoSuchMethodException) {
            // Fall through to connect(int)
        } catch (e: InvocationTargetException) {
            throw e.targetException ?: e
        }

        try {
            val connectFlags = UiAutomation::class.java.getDeclaredMethod("connect", Int::class.javaPrimitiveType)
            connectFlags.isAccessible = true
            connectFlags.invoke(uia, 0)
        } catch (e: InvocationTargetException) {
            throw e.targetException ?: e
        }
    }

    private fun configureServiceInfo(uia: UiAutomation) {
        try {
            val info = uia.serviceInfo ?: AccessibilityServiceInfo()
            var flags = info.flags
            flags = flags or AccessibilityServiceInfo.FLAG_RETRIEVE_INTERACTIVE_WINDOWS
            flags = flags or AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS
            if (config.includeUnimportant) {
                flags = flags or AccessibilityServiceInfo.FLAG_INCLUDE_NOT_IMPORTANT_VIEWS
            } else {
                flags = flags and AccessibilityServiceInfo.FLAG_INCLUDE_NOT_IMPORTANT_VIEWS.inv()
            }
            info.flags = flags
            info.eventTypes = AccessibilityEvent.TYPES_ALL_MASK
            uia.serviceInfo = info
        } catch (t: Throwable) {
            if (config.verbose) {
                System.err.println("[ShadowGraph] configureServiceInfo warning: ${t.message}")
            }
        }
    }

    fun disconnect() {
        if (!disconnected.compareAndSet(false, true)) return
        val uia = uiAutomation
        uiAutomation = null
        if (uia != null) {
            try {
                uia.setOnAccessibilityEventListener(null)
            } catch (_: Throwable) {
            }
            try {
                val disconnectMethod = UiAutomation::class.java.getDeclaredMethod("disconnect")
                disconnectMethod.isAccessible = true
                disconnectMethod.invoke(uia)
            } catch (t: Throwable) {
                if (config.verbose) {
                    System.err.println("[ShadowGraph] disconnect warning: ${t.message}")
                }
            }
        }
        try {
            looperThread.quitSafely()
        } catch (_: Throwable) {
        }
    }
}
