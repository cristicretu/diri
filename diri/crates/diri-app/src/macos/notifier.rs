//! macOS delivery and navigation. A notification is a route to a session,
//! never a delayed permission keystroke into an arbitrary terminal prompt.
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSArray, NSBundle, NSError, NSObject, NSObjectProtocol, NSSet, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction,
    UNNotificationActionOptions, UNNotificationCategory, UNNotificationCategoryOptions,
    UNNotificationDismissActionIdentifier, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};
use tokio::sync::mpsc;

use crate::notifications::{NotificationRequest as DiriNotification, OPEN_ACTION_ID};

#[derive(Clone, Debug)]
pub enum NativeNotificationEvent {
    Open {
        session_id: String,
        notification_id: String,
    },
    Read(String),
    Health(String),
}

pub struct NativeNotifier {
    inner: Option<NativeNotifierInner>,
    badge_count: Cell<Option<usize>>,
}
struct NativeNotifierInner {
    center: Retained<UNUserNotificationCenter>,
    _delegate: Retained<NotificationDelegate>,
    sender: mpsc::UnboundedSender<NativeNotificationEvent>,
    authorization_requested: Cell<bool>,
    pending: RefCell<BTreeMap<String, Arc<AtomicBool>>>,
}

impl NativeNotifier {
    pub fn new(sender: mpsc::UnboundedSender<NativeNotificationEvent>) -> Self {
        let inner = NSBundle::mainBundle().bundleIdentifier().map(|_| {
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let delegate = NotificationDelegate::new(sender.clone());
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            let open = UNNotificationAction::actionWithIdentifier_title_options(
                &NSString::from_str(OPEN_ACTION_ID),
                &NSString::from_str("Open session"),
                UNNotificationActionOptions::Foreground,
            );
            let category =
                UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
                    &NSString::from_str("diri-session"),
                    &NSArray::from_retained_slice(&[open]),
                    &NSArray::new(),
                    UNNotificationCategoryOptions::CustomDismissAction,
                );
            center.setNotificationCategories(&NSSet::from_retained_slice(&[category]));
            NativeNotifierInner {
                center,
                _delegate: delegate,
                sender: sender.clone(),
                authorization_requested: Cell::new(false),
                pending: RefCell::new(BTreeMap::new()),
            }
        });
        if inner.is_none() {
            let _ = sender.send(NativeNotificationEvent::Health(
                "Mac alerts require the packaged Diri app. Your inbox still works.".into(),
            ));
        }
        Self {
            inner,
            badge_count: Cell::new(None),
        }
    }

    pub fn set_badge(&self, count: usize) {
        if self.badge_count.replace(Some(count)) == Some(count) {
            return;
        }
        if let Some(main) = objc2::MainThreadMarker::new() {
            let label = (count != 0).then(|| NSString::from_str(&count.to_string()));
            objc2_app_kit::NSApplication::sharedApplication(main)
                .dockTile()
                .setBadgeLabel(label.as_deref());
        }
    }

    pub fn refresh_health(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        let sender = inner.sender.clone();
        let completion = RcBlock::new(
            move |settings: std::ptr::NonNull<objc2_user_notifications::UNNotificationSettings>| {
                // SAFETY: the notification center supplies a valid settings object for the callback.
                let settings = unsafe { settings.as_ref() };
                use objc2_user_notifications::UNAuthorizationStatus as Status;
                let message = match settings.authorizationStatus() {
                    Status::Denied => {
                        "Mac alerts are disabled. Enable Diri in System Settings → Notifications."
                    }
                    Status::NotDetermined => {
                        "Use Test alert to enable Mac alerts. Your inbox already works."
                    }
                    _ => {
                        "Mac alerts are enabled. Focus mode and System Settings can silence delivery."
                    }
                };
                let _ = sender.send(NativeNotificationEvent::Health(message.into()));
            },
        );
        inner
            .center
            .getNotificationSettingsWithCompletionHandler(&completion);
    }

    pub fn dismiss(&self, identifiers: &[String]) {
        if let Some(inner) = &self.inner {
            for id in identifiers {
                if let Some(active) = inner.pending.borrow_mut().remove(id) {
                    active.store(false, Ordering::SeqCst);
                }
            }
            let ids = NSArray::from_retained_slice(
                &identifiers
                    .iter()
                    .map(|id| NSString::from_str(id))
                    .collect::<Vec<_>>(),
            );
            inner
                .center
                .removePendingNotificationRequestsWithIdentifiers(&ids);
            inner
                .center
                .removeDeliveredNotificationsWithIdentifiers(&ids);
        }
    }

    pub fn post(&self, notification: &DiriNotification) {
        let Some(inner) = &self.inner else { return };
        let active = Arc::new(AtomicBool::new(true));
        {
            let mut pending = inner.pending.borrow_mut();
            if pending.len() >= 200
                && let Some((_, oldest)) = pending.pop_first()
            {
                oldest.store(false, Ordering::SeqCst);
            }
            if let Some(previous) = pending.insert(notification.identifier.clone(), active.clone())
            {
                previous.store(false, Ordering::SeqCst);
            }
        }
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&notification.title));
        content.setBody(&NSString::from_str(&notification.body));
        if let Some(session) = &notification.thread_identifier {
            content.setThreadIdentifier(&NSString::from_str(session));
            content.setCategoryIdentifier(&NSString::from_str("diri-session"));
        }
        if notification.use_system_sound {
            content.setSound(Some(&UNNotificationSound::defaultSound()));
        }
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&notification.identifier),
            &content,
            None,
        );
        if !inner.authorization_requested.replace(true) {
            let sender = inner.sender.clone();
            let completion = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
                let _ = sender.send(NativeNotificationEvent::Health(if granted.as_bool() {
                    "Mac alerts are enabled. Focus mode and System Settings can silence delivery.".into()
                } else {
                    "Mac alerts are disabled. Enable Diri in System Settings → Notifications. Your inbox still works.".into()
                }));
                if granted.as_bool() {
                    deliver(&request, sender.clone(), active.clone());
                }
            });
            inner
                .center
                .requestAuthorizationWithOptions_completionHandler(
                    UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                    &completion,
                );
        } else {
            deliver(&request, inner.sender.clone(), active);
        }
    }
}

fn deliver(
    request: &UNNotificationRequest,
    sender: mpsc::UnboundedSender<NativeNotificationEvent>,
    active: Arc<AtomicBool>,
) {
    if !active.load(Ordering::SeqCst) {
        return;
    }
    let id = request.identifier().to_string();
    let completion = RcBlock::new(move |error: *mut NSError| {
        if !active.load(Ordering::SeqCst) {
            let ids = NSArray::from_retained_slice(&[NSString::from_str(&id)]);
            let center = UNUserNotificationCenter::currentNotificationCenter();
            center.removePendingNotificationRequestsWithIdentifiers(&ids);
            center.removeDeliveredNotificationsWithIdentifiers(&ids);
        }
        if !error.is_null() {
            let _ = sender.send(NativeNotificationEvent::Health(
                "macOS couldn't deliver an alert. Your notifications are available in the inbox."
                    .into(),
            ));
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(request, Some(&completion));
}

struct DelegateIvars {
    sender: mpsc::UnboundedSender<NativeNotificationEvent>,
}
define_class!(
    // SAFETY: NSObject has no subclassing requirements. Delegate callbacks may
    // arrive off-main; they only enqueue owned values for the GPUI task.
    #[unsafe(super(NSObject))]
    #[ivars = DelegateIvars]
    struct NotificationDelegate;
    unsafe impl NSObjectProtocol for NotificationDelegate {}
    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &block2::DynBlock<dyn Fn()>,
        ) {
            let request = response.notification().request();
            let id = request.identifier().to_string();
            let event = if &*response.actionIdentifier()
                == unsafe { UNNotificationDismissActionIdentifier }
            {
                NativeNotificationEvent::Read(id)
            } else {
                // Also covers old Approve/Deny banners delivered by a previous
                // app version. They open the session and never send input.
                NativeNotificationEvent::Open {
                    session_id: request.content().threadIdentifier().to_string(),
                    notification_id: id,
                }
            };
            let _ = self.ivars().sender.send(event);
            completion.call(());
        }
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }
    }
);
impl NotificationDelegate {
    fn new(sender: mpsc::UnboundedSender<NativeNotificationEvent>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { sender });
        // SAFETY: NSObject init is its designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}
