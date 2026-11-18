package com.maxa.device.companion

import org.junit.Assert.*
import org.junit.Test

class RetiredEmbeddedApiMigrationTest {
    @Test fun absentReadGrantNeedsNoRelease() {
        assertTrue(retireRecordedDirectoryReadGrant({ false }, { error("must not release") }))
    }

    @Test fun matchingReadGrantMustBeGoneBeforeForgettingRow() {
        var held = true
        assertTrue(retireRecordedDirectoryReadGrant({ held }, { held = false }))
        assertFalse(held)
    }

    @Test fun failedReleaseWithGrantStillPresentKeepsRow() {
        assertFalse(retireRecordedDirectoryReadGrant({ true }, { throw SecurityException() }))
    }

    @Test fun unverifiableReleaseKeepsRowForRetry() {
        var released = false
        assertFalse(retireRecordedDirectoryReadGrant({ if (released) throw IllegalStateException() else true }, { released = true }))
        assertTrue(released)
    }
}
