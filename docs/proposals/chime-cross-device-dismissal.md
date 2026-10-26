# Chime: change of plan for cross-device dismissal

## Decision

Attempt cross-device notification dismissal through background Web Push. Assume the Chime page is closed during normal use.

The original proposal remains unchanged. This document replaces its planned approach to cross-device dismissal and the related event-stream implementation and verification requirements.

## Why change

The proposal uses an open page and an authenticated SSE stream to close notifications on other devices, with reconciliation when the page reopens. That does not match the intended use: devices should receive and dismiss notifications without keeping the management page open.

A second push can wake a service worker and ask it to close a notification. Browser restrictions make this best-effort, but trying it is more useful than depending on an open page.

## Revised flow

1. Chime sends an ordinary notification to enrolled devices, including its ID and acknowledgement capability.
2. Clicking it closes it locally and opens its optional URL immediately. The service worker sends the authenticated acknowledgement using `event.waitUntil()`.
3. Chime records the acknowledgement and attempts an encrypted `dismiss` push to the other original recipients that remain enrolled.
4. Each receiving service worker handles the push without opening a page, finds notifications with the matching tag using `registration.getNotifications({ tag })`, and calls `close()` on them.

Use explicit, versioned `notify` and `dismiss` payload variants. A dismissal needs only the notification ID and expiry. A valid dismissal with no matching notification is a no-op; it must not become a generic application notification or trigger another acknowledgement.

Keep the existing device-token and notification-capability checks. Store original recipient IDs alongside the short-lived acknowledgement record. Atomically mark the first acknowledgement so repeated requests do not repeat dismissal fan-out. Reuse bounded push concurrency, timeouts and expired-subscription cleanup. Do not add a durable retry queue.

Remove the planned SSE endpoint, browser stream parser and persistent page connection. Page-opening reconciliation can remain as opportunistic cleanup, but it is not the normal dismissal path.

## Chrome tradeoff

Chrome is the initial target for background-dismissal testing on desktop and Android. Its service worker can receive a push and close an existing notification while the page is closed.

However, closing a notification does not reliably satisfy Chrome's user-visible push requirement. When no notifications remain, Chrome may permit the event using its silent-push budget or display its own generic background-update notification. Accept that possible replacement notification as a tradeoff and report observed behavior clearly.

Continue subscribing with `userVisibleOnly: true`. Do not use show-then-close tricks to evade browser enforcement. Do not promise that dismissals will always be silent or immediately delivered.

Safari has stronger restrictions on non-visible pushes, including possible subscription revocation. Verify it separately before enabling or claiming background-dismissal support there. Ordinary notification delivery and background dismissal have separate compatibility results.

## Ordering and failure behavior

Remember dismissed IDs and their original expiries in a bounded IndexedDB set, including on the clicked device. Serialize display/dismissal handling so a delayed ordinary push does not redisplay an ID already remembered as dismissed. These records contain no notification content and expire with the original notification lifetime.

Dismissal pushes use the remaining original lifetime as their TTL. An acknowledgement response confirms server acknowledgement, not removal from another device's OS.

If the clicked device cannot reach Chime, its notification still closes locally; cross-device acknowledgement is lost. Missed pushes, server restarts and expired or evicted records can leave notifications visible. A late notification can reappear if its dismissal record is gone. No offline acknowledgement queue is introduced.

Push TTL limits provider retention of undelivered messages. It does not set an expiry on an already displayed OS notification.

## Verification

- Close all Chime pages on receiving devices. Click a notification on one device and verify the other service worker receives a dismissal and closes the matching notification.
- Test Chrome desktop and Android, recording browser version, OS and any generic replacement notification. Repeat with only one notification remaining, multiple notifications and low site engagement.
- Exercise duplicate acknowledgements, unmatched dismissals, delayed and concurrent delivery, expired records, device removal, offline devices and restarts. Confirm bounded fan-out and no acknowledgement loops.
- Confirm ordinary notification delivery still works when dismissal fails. Check Safari separately before enabling background dismissal there.

Success means Chime attempts useful cross-device dismissal with its pages closed, and documents the actual browser behavior. Guaranteed silent removal is not required.

## Reference

[Chromium push-notification enforcement](https://chromium.googlesource.com/chromium/src/+/main/chrome/browser/push_messaging/push_messaging_notification_manager.cc) contains the visible-notification check, silent-push budget and generic-notification fallback. These implementation details are browser behavior to verify, not an application delivery guarantee.
