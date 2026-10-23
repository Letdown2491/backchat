//! Desktop notifications through the xdg-desktop-portal Notification
//! portal. The portal is the path that works both inside the flatpak
//! sandbox (portal names are always reachable there, no finish-args) and
//! bare on the host. Fire and forget: a chat ping must never block or
//! crash over the notification service, and once the portal is missing
//! (bare compositor with no portal service) we stop asking.

/// A notification the store wants on screen.
#[derive(Debug, Clone)]
pub struct Ping {
    /// Stable id: a later ping with the same id replaces the old one
    /// instead of stacking (one per peer, one for mentions).
    pub id: String,
    pub title: String,
    pub body: String,
}

static PORTAL_DEAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Send a notification on the tokio runtime; never blocks the caller.
pub fn send(handle: &tokio::runtime::Handle, ping: Ping) {
    use std::sync::atomic::Ordering;
    if PORTAL_DEAD.load(Ordering::Relaxed) {
        return;
    }
    handle.spawn(async move {
        use ashpd::desktop::notification::{Notification, NotificationProxy};
        let proxy = match NotificationProxy::new().await {
            Ok(proxy) => proxy,
            Err(e) => {
                eprintln!("backchat: notification portal unavailable, notifications off: {e}");
                PORTAL_DEAD.store(true, Ordering::Relaxed);
                return;
            }
        };
        let notification = Notification::new(ping.title.as_str()).body(ping.body.as_str());
        if let Err(e) = proxy.add_notification(&ping.id, notification).await {
            eprintln!("backchat: notification failed: {e}");
        }
    });
}
