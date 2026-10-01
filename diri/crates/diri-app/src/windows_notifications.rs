//! Unpackaged desktop toasts use the installer's Start-menu AppUserModelID.
//! Callbacks contain identifiers only; replies go through the shared admission
//! check on the application thread. No permission keystrokes are synthesized.
use crate::{
    native_notifications::{NativeNotificationEvent, ReplyText},
    notifications::NotificationRequest,
};
use std::{cell::RefCell, collections::BTreeMap, sync::atomic::Ordering};
use tokio::sync::mpsc::UnboundedSender;
use windows::{
    Data::Xml::Dom::XmlDocument,
    Foundation::{IPropertyValue, TypedEventHandler},
    UI::Notifications::{
        ToastActivatedEventArgs, ToastDismissalReason, ToastNotification, ToastNotificationManager,
        ToastNotifier,
    },
    core::{HSTRING, Interface},
};

const APP_ID: &str = "com.dirijor.diri";
pub struct NativeNotifier {
    notifier: windows::core::Result<ToastNotifier>,
    sender: UnboundedSender<NativeNotificationEvent>,
    live: RefCell<BTreeMap<String, ToastNotification>>,
}
fn xml(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
impl NativeNotifier {
    pub fn new(sender: UnboundedSender<NativeNotificationEvent>) -> Self {
        Self {
            notifier: ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID)),
            sender,
            live: RefCell::new(BTreeMap::new()),
        }
    }
    pub fn refresh_health(&self) {
        let message = match &self.notifier {
            Ok(notifier)
                if notifier.Setting().is_ok_and(|value| {
                    value == windows::UI::Notifications::NotificationSetting::Enabled
                }) =>
            {
                "Windows alerts are enabled. Use Test alert to check delivery."
            }
            _ => {
                "Windows alerts are unavailable or disabled. Check notification settings and Diri's Start menu installation. Your inbox still works."
            }
        };
        let _ = self
            .sender
            .send(NativeNotificationEvent::Health(message.into()));
    }
    pub fn dismiss(&self, ids: &[String]) {
        if let Ok(notifier) = &self.notifier {
            let mut live = self.live.borrow_mut();
            for id in ids {
                if let Some(toast) = live.remove(id) {
                    let _ = notifier.Hide(&toast);
                }
            }
        }
    }
    pub fn post(&self, request: &NotificationRequest) {
        if request
            .guard
            .as_ref()
            .is_some_and(|guard| !guard.0.load(Ordering::SeqCst))
        {
            return;
        }
        self.dismiss(std::slice::from_ref(&request.identifier));
        if let Err(error) = self.post_inner(request) {
            // HRESULT, never the notification body or reply.
            let _ = self.sender.send(NativeNotificationEvent::Health(format!("Windows could not deliver the alert ({:#x}). Check Settings › System › Notifications and install Diri's Start menu shortcut. Your inbox still works.", error.code().0)));
        } else {
            let _ = self.sender.send(NativeNotificationEvent::Health("Alert sent to Windows. Focus and notification settings may silence it; your inbox still works.".into()));
        }
    }
    fn post_inner(&self, request: &NotificationRequest) -> windows::core::Result<()> {
        let notifier = self.notifier.as_ref().map_err(Clone::clone)?;
        let actions = if request.reply && request.thread_identifier.is_some() {
            r#"<actions><input id="reply" type="text" placeHolderContent="Reply to the agent"/><action content="Send" arguments="reply" activationType="foreground" hint-inputId="reply"/></actions>"#
        } else {
            ""
        };
        let audio = if request.use_system_sound {
            ""
        } else {
            r#"<audio silent="true"/>"#
        };
        let document = XmlDocument::new()?;
        document.LoadXml(&HSTRING::from(format!(r#"<toast><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual>{actions}{audio}</toast>"#, xml(&request.title), xml(&request.body))))?;
        let toast = ToastNotification::CreateToastNotification(&document)?;
        let sender = self.sender.clone();
        let id = request.identifier.clone();
        let session = request.thread_identifier.clone();
        toast.Activated(&TypedEventHandler::new(
            move |_, args: windows::core::Ref<'_, windows::core::IInspectable>| {
                if let Some(session_id) = &session {
                    let reply = args
                        .as_ref()
                        .and_then(|args| args.cast::<ToastActivatedEventArgs>().ok())
                        .filter(|args| args.Arguments().is_ok_and(|arg| arg == "reply"))
                        .and_then(|args| args.UserInput().ok())
                        .and_then(|input| input.Lookup(&HSTRING::from("reply")).ok())
                        .and_then(|value| value.cast::<IPropertyValue>().ok())
                        .and_then(|value| value.GetString().ok());
                    let event = match reply {
                        Some(text) => NativeNotificationEvent::Reply {
                            session_id: session_id.clone(),
                            notification_id: id.clone(),
                            text: ReplyText(text.to_string()),
                        },
                        None => NativeNotificationEvent::Open {
                            session_id: session_id.clone(),
                            notification_id: id.clone(),
                        },
                    };
                    let _ = sender.send(event);
                }
                Ok(())
            },
        ))?;
        let sender = self.sender.clone();
        let id = request.identifier.clone();
        toast.Dismissed(&TypedEventHandler::new(
            move |_,
                  args: windows::core::Ref<
                '_,
                windows::UI::Notifications::ToastDismissedEventArgs,
            >| {
                if args.as_ref().is_some_and(|args| {
                    args.Reason()
                        .is_ok_and(|reason| reason == ToastDismissalReason::UserCanceled)
                }) {
                    let _ = sender.send(NativeNotificationEvent::Read(id.clone()));
                }
                Ok(())
            },
        ))?;
        let sender = self.sender.clone();
        toast.Failed(&TypedEventHandler::new(move |_, _: windows::core::Ref<'_, windows::UI::Notifications::ToastFailedEventArgs>| {
            let _ = sender.send(NativeNotificationEvent::Health("Windows rejected the alert. Check notification settings; your inbox still works.".into()));
            Ok(())
        }))?;
        let mut live = self.live.borrow_mut();
        if live.len() >= 200
            && let Some((_, old)) = live.pop_first()
        {
            let _ = notifier.Hide(&old);
        }
        notifier.Show(&toast)?;
        live.insert(request.identifier.clone(), toast);
        Ok(())
    }
}
