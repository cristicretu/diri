//! AppKit indirect-touch seam. No event monitor, swizzle, global hook, or
//! synthetic mouse/scroll event. Unhandled events retain the responder chain.
use crate::tab_peek::{GestureFrame, ThreeFingerGesture};
use objc2::rc::Retained;
use objc2::{ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSEvent, NSResponder, NSTouchPhase, NSTouchTypeMask, NSView};
use objc2_foundation::NSObjectProtocol;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::RefCell;

struct GestureIvars {
    view: Retained<NSView>,
    gesture: RefCell<ThreeFingerGesture>,
    frames: tokio::sync::watch::Sender<GestureFrame>,
}
define_class!(
    #[unsafe(super(NSResponder))]
    #[thread_kind = MainThreadOnly]
    #[ivars = GestureIvars]
    struct GestureResponder;
    unsafe impl NSObjectProtocol for GestureResponder {}
    impl GestureResponder {
        #[unsafe(method(touchesBeganWithEvent:))]
        fn began(&self, event: &NSEvent) {
            self.sample(event, false);
            // Observe the touch stream without consuming another responder's gesture.
            unsafe { let _: () = msg_send![super(self), touchesBeganWithEvent: event]; }
        }
        #[unsafe(method(touchesMovedWithEvent:))]
        fn moved(&self, event: &NSEvent) {
            self.sample(event, false);
            // Observe the touch stream without consuming another responder's gesture.
            unsafe { let _: () = msg_send![super(self), touchesMovedWithEvent: event]; }
        }
        #[unsafe(method(touchesEndedWithEvent:))]
        fn ended(&self, event: &NSEvent) {
            self.sample(event, false);
            // Observe the touch stream without consuming another responder's gesture.
            unsafe { let _: () = msg_send![super(self), touchesEndedWithEvent: event]; }
        }
        #[unsafe(method(touchesCancelledWithEvent:))]
        fn cancelled(&self, event: &NSEvent) {
            self.sample(event, true);
            // Observe the touch stream without consuming another responder's gesture.
            unsafe { let _: () = msg_send![super(self), touchesCancelledWithEvent: event]; }
        }
    }
);
impl GestureResponder {
    fn sample(&self, event: &NSEvent, cancelled: bool) {
        let touches =
            event.touchesMatchingPhase_inView(NSTouchPhase::Touching, Some(&self.ivars().view));
        let contacts = touches
            .iter()
            .map(|touch| {
                let position = touch.normalizedPosition();
                let identity = touch.identity();
                (
                    Retained::as_ptr(&identity) as usize as u64,
                    position.x as f32,
                    position.y as f32,
                )
            })
            .collect();
        if let Some(frame) = self
            .ivars()
            .gesture
            .borrow_mut()
            .sample(contacts, cancelled)
        {
            self.ivars().frames.send_replace(frame);
        }
    }
}

/// Owned for exactly one GPUI window. Drop restores only a chain still owned
/// by us; it never overwrites a responder installed by another component.
pub(crate) struct TabGestureBridge {
    view: Retained<NSView>,
    responder: Retained<GestureResponder>,
    previous: Option<Retained<NSResponder>>,
    previous_touch_types: NSTouchTypeMask,
}
impl TabGestureBridge {
    pub(crate) fn cancel(&self) {
        self.responder
            .ivars()
            .gesture
            .borrow_mut()
            .sample(Vec::new(), true);
        self.responder
            .ivars()
            .frames
            .send_replace(GestureFrame::Cancelled);
    }

    pub(crate) fn install(
        window: &impl HasWindowHandle,
    ) -> Option<(Self, tokio::sync::watch::Receiver<GestureFrame>)> {
        let marker = MainThreadMarker::new()?;
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return None;
        };
        // GPUI guarantees this NSView remains valid for the window lifetime;
        // retain it so teardown remains safe even during window destruction.
        let view = unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }?;
        let previous = unsafe { view.nextResponder() };
        let previous_touch_types = view.allowedTouchTypes();
        let (frames, receiver) = tokio::sync::watch::channel(GestureFrame::Cancelled);
        let responder = marker.alloc().set_ivars(GestureIvars {
            view: view.clone(),
            gesture: Default::default(),
            frames,
        });
        let responder: Retained<GestureResponder> = unsafe { msg_send![super(responder), init] };
        unsafe {
            responder.setNextResponder(previous.as_deref());
            view.setNextResponder(Some(&responder));
        }
        view.setAllowedTouchTypes(previous_touch_types | NSTouchTypeMask::Indirect);
        Some((
            Self {
                view,
                responder,
                previous,
                previous_touch_types,
            },
            receiver,
        ))
    }
}
impl Drop for TabGestureBridge {
    fn drop(&mut self) {
        unsafe {
            // Another component may have inserted a responder ahead of ours.
            // Unlink just our node, preserving that component's chain and mask.
            let mut cursor = Some(self.view.clone().into_super());
            let mut visited = std::collections::HashSet::new();
            while let Some(predecessor) = cursor {
                if !visited.insert(Retained::as_ptr(&predecessor) as usize) {
                    break;
                }
                let next = predecessor.nextResponder();
                if next
                    .as_deref()
                    .is_some_and(|next| std::ptr::eq(next, self.responder.as_super()))
                {
                    predecessor.setNextResponder(self.previous.as_deref());
                    break;
                }
                cursor = next;
            }
            if self.view.allowedTouchTypes()
                == self.previous_touch_types | NSTouchTypeMask::Indirect
            {
                self.view.setAllowedTouchTypes(self.previous_touch_types);
            }
            self.responder.setNextResponder(None);
        }
    }
}
