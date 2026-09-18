//! Application-owned delivery survives closing every workbench window. Native
//! callbacks carry identifiers onto the main thread; they never retain a Root.
#[cfg(target_os = "macos")]
use std::rc::Rc;
use std::{sync::Arc, time::Instant};

#[cfg(target_os = "macos")]
use crate::macos::notifier::{NativeNotificationEvent, NativeNotifier};
use crate::notifications::NotificationSound;
use crate::sounds::{self, PlatformPlayer, SoundGate, StatusSound};
use crate::{AppServices, sidebar::PreviewScenario};
use gpui::{App, Global};

pub(crate) struct ApplicationNotifications {
    #[cfg(target_os = "macos")]
    notifier: Rc<NativeNotifier>,
    #[cfg(target_os = "macos")]
    health: String,
}
impl Global for ApplicationNotifications {}

pub(crate) fn install(
    services: Arc<AppServices>,
    preview: bool,
    scenario: PreviewScenario,
    cx: &mut App,
) {
    if cx.has_global::<ApplicationNotifications>() {
        return;
    }
    #[cfg(target_os = "macos")]
    let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
    #[cfg(target_os = "macos")]
    let notifier = Rc::new(NativeNotifier::new(sender));
    #[cfg(target_os = "macos")]
    notifier.set_badge(
        services
            .store
            .store
            .read()
            .expect("store")
            .notifications()
            .unread_count(),
    );
    cx.set_global(ApplicationNotifications {
        #[cfg(target_os = "macos")]
        notifier: notifier.clone(),
        #[cfg(target_os = "macos")]
        health: "Use Test alert to check Mac notification delivery.".into(),
    });
    #[cfg(target_os = "macos")]
    {
        let services = services.clone();
        cx.spawn(async move |cx| {
            while let Some(event) = events.recv().await {
                cx.update(|cx| route(event, &services, preview, scenario, cx));
            }
        })
        .detach();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (preview, scenario);
    let mut events = services.store.status_events();
    let mut changes = services.store.changes();
    cx.spawn(async move |cx| {
        let mut sound_gate = SoundGate::default();
        loop {
            tokio::select! {
                event = events.recv() => {
                    let event = match event {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
                    let (deliver, active) = {
                        let store = services.store.store.read().expect("store");
                        (event.notification.as_ref().is_none_or(|request|store.should_deliver_notification(request)), store.app_is_active())
                    };
                    if let Some(sound) = event.sound.filter(|_|deliver) {
                        let sound = match sound {
                            NotificationSound::NeedsInput => StatusSound::NeedsInput,
                            NotificationSound::Done => StatusSound::Done,
                            NotificationSound::Frozen => StatusSound::Frozen,
                        };
                        if sound_gate.should_play(sound, Instant::now()) { let _ = sounds::play(&PlatformPlayer, sound); }
                    }
                    #[cfg(target_os = "macos")]
                    {
                        notifier.dismiss(&event.dismiss);
                        if let Some(notification) = event.notification.filter(|_|deliver && (!active || event.in_app_banner.is_none())) { notifier.post(&notification); }
                    }
                    #[cfg(not(target_os = "macos"))]
                    let _ = active;
                }
                change = changes.recv() => {
                    if matches!(change, Err(tokio::sync::broadcast::error::RecvError::Closed)) { break; }
                    #[cfg(target_os = "macos")]
                    notifier.set_badge(services.store.store.read().expect("store").notifications().unread_count());
                }
            }
            // Keep the application task tied to GPUI's lifetime, including
            // periods with zero windows.
            cx.update(|_| ());
        }
    }).detach();
}

#[cfg(target_os = "macos")]
pub(crate) fn notifier(cx: &App) -> Rc<NativeNotifier> {
    cx.global::<ApplicationNotifications>().notifier.clone()
}
#[cfg(target_os = "macos")]
pub(crate) fn health(cx: &App) -> String {
    cx.global::<ApplicationNotifications>().health.clone()
}

#[cfg(target_os = "macos")]
pub(crate) fn route(
    event: NativeNotificationEvent,
    services: &Arc<AppServices>,
    preview: bool,
    scenario: PreviewScenario,
    cx: &mut App,
) {
    use crate::store::{WindowAction, WindowStore};
    match event {
        NativeNotificationEvent::Open {
            session_id,
            notification_id,
        } => {
            let action = WindowAction::OpenNotification {
                session: diri_proto::SessionId::new(session_id),
                notification: notification_id,
            };
            if WindowStore::focused(&services.store.store)
                .is_some_and(|window| window.enqueue(action.clone()))
            {
                return;
            }
            // No live recipient (or its bounded action queue is saturated).
            // Open a fresh workbench and retain the exact notification target.
            let window = crate::open_main_window(
                cx,
                services.clone(),
                preview,
                scenario,
                crate::window_restore::RestorePolicy::FRAME_ONLY,
            );
            let WindowAction::OpenNotification {
                session,
                notification,
            } = action
            else {
                unreachable!()
            };
            let _ = window.update(cx, |root, window, cx| {
                root.open_notification(session, Some(notification), window, cx)
            });
        }
        NativeNotificationEvent::Read(id) => services
            .store
            .store
            .write()
            .expect("store")
            .set_notification_read(&id, true),
        NativeNotificationEvent::Health(message) => {
            cx.global_mut::<ApplicationNotifications>().health = message;
            cx.refresh_windows();
        }
    }
}
