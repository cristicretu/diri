//! AppKit indirect-touch seam. No event monitor, swizzle, global hook, or
//! synthetic mouse/scroll event. Unhandled events retain the responder chain.
use crate::gesture_delivery::{GestureReceiver, GestureSender};
#[cfg(test)]
use crate::tab_peek::GestureFrame;
use crate::tab_peek::ThreeFingerGesture;
use objc2::rc::Retained;
use objc2::{ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSEvent, NSResponder, NSTouchPhase, NSTouchTypeMask, NSView};
use objc2_foundation::NSObjectProtocol;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::{Cell, RefCell};

struct GestureIvars {
    view: Retained<NSView>,
    gesture: RefCell<ThreeFingerGesture>,
    revealed: Cell<bool>,
    frames: GestureSender,
    suppress_until_lift: Cell<bool>,
    contacts_present: Cell<bool>,
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
        if cancelled {
            self.sample_contacts(Vec::new(), true);
            return;
        }
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
        self.sample_contacts(contacts, cancelled);
    }

    fn sample_contacts(&self, contacts: Vec<(u64, f32, f32)>, cancelled: bool) {
        self.ivars().contacts_present.set(!contacts.is_empty());
        if self.ivars().suppress_until_lift.get() && !cancelled {
            if contacts.is_empty() {
                self.ivars().suppress_until_lift.set(false);
                self.ivars().gesture.borrow_mut().sample(Vec::new(), false);
            }
            return;
        }
        let lifted = contacts.is_empty();
        let frame = self.ivars().gesture.borrow_mut().sample_with_reverse(
            contacts,
            cancelled,
            self.ivars().revealed.get(),
        );
        if let Some(frame) = frame
            && !self.ivars().frames.send(frame, std::time::Instant::now())
        {
            self.ivars().suppress_until_lift.set(!lifted);
            self.ivars()
                .gesture
                .borrow_mut()
                .sample(Vec::new(), !lifted);
        }
    }
}

/// Owned for exactly one GPUI window. Drop restores only a chain still owned
/// by us; it never overwrites a responder installed by another component.
pub(crate) struct TabGestureBridge {
    view: Retained<NSView>,
    responder: Retained<GestureResponder>,
    _previous: Option<Retained<NSResponder>>,
    previous_touch_types: NSTouchTypeMask,
}
impl TabGestureBridge {
    /// Presentation state only; no polling or extra event interception.
    pub(crate) fn set_revealed(&self, revealed: bool) {
        self.responder.ivars().revealed.set(revealed);
    }

    pub(crate) fn cancel(&self) {
        let contacts_present = self.responder.ivars().contacts_present.get();
        self.responder
            .ivars()
            .gesture
            .borrow_mut()
            .sample(Vec::new(), contacts_present);
        self.responder
            .ivars()
            .frames
            .cancel(std::time::Instant::now());
        self.responder
            .ivars()
            .suppress_until_lift
            .set(contacts_present);
    }

    pub(crate) fn install(window: &impl HasWindowHandle) -> Option<(Self, GestureReceiver)> {
        let marker = MainThreadMarker::new()?;
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return None;
        };
        // GPUI guarantees this NSView remains valid for the window lifetime;
        // retain it so teardown remains safe even during window destruction.
        let view = unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }?;
        Some(Self::install_view(marker, view))
    }

    fn install_view(marker: MainThreadMarker, view: Retained<NSView>) -> (Self, GestureReceiver) {
        let previous = unsafe { view.nextResponder() };
        let previous_touch_types = view.allowedTouchTypes();
        let (frames, receiver) = crate::gesture_delivery::channel();
        let responder = marker.alloc().set_ivars(GestureIvars {
            view: view.clone(),
            gesture: Default::default(),
            revealed: Cell::new(false),
            frames,
            suppress_until_lift: Cell::new(false),
            contacts_present: Cell::new(false),
        });
        let responder: Retained<GestureResponder> = unsafe { msg_send![super(responder), init] };
        unsafe {
            responder.setNextResponder(previous.as_deref());
            view.setNextResponder(Some(&responder));
        }
        view.setAllowedTouchTypes(previous_touch_types | NSTouchTypeMask::Indirect);
        (
            Self {
                view,
                responder,
                _previous: previous,
                previous_touch_types,
            },
            receiver,
        )
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
                    predecessor.setNextResponder(self.responder.nextResponder().as_deref());
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

#[cfg(test)]
define_class!(
    #[unsafe(super(NSResponder))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Cell<usize>]
    struct TouchForwarder;
    unsafe impl NSObjectProtocol for TouchForwarder {}
    impl TouchForwarder {
        #[unsafe(method(touchesCancelledWithEvent:))]
        fn cancelled(&self, _: &NSEvent) {
            self.ivars().set(self.ivars().get()+1);
        }
    }
);

// A harness-free integration test runs this on the actual AppKit main thread.
// Ordinary libtest worker threads cannot exercise this responder ownership seam.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn verify_appkit_bridge() {
    let marker = MainThreadMarker::new().expect("AppKit test must run on the main thread");
    verify_coalesced_stroke_boundaries(marker);
    verify_overflow_waits_for_lift(marker);
    let view = NSView::new(marker);
    let previous = marker.alloc::<TouchForwarder>().set_ivars(Cell::new(0));
    let previous: Retained<TouchForwarder> = unsafe { msg_send![super(previous), init] };
    unsafe {
        view.setNextResponder(Some(&previous));
    }
    let original_mask = view.allowedTouchTypes();
    let (bridge, mut frames) = TabGestureBridge::install_view(marker, view.clone());
    assert!(view.allowedTouchTypes().contains(NSTouchTypeMask::Indirect));
    assert!(std::ptr::eq(
        unsafe { view.nextResponder() }.as_deref().unwrap(),
        bridge.responder.as_super()
    ));
    let contacts = |y| (1..=3).map(|id| (id, 0.5, y)).collect();
    bridge.responder.sample_contacts(contacts(0.7), false);
    assert!(frames.take_pending().is_none());
    bridge.responder.sample_contacts(contacts(0.5), false);
    assert!(
        matches!(frames.take_pending().unwrap().iter().last().unwrap().frame, GestureFrame::Tracking(distance) if (distance-240.0).abs()<0.01)
    );
    bridge.responder.sample_contacts(Vec::new(), false);
    assert!(
        matches!(frames.take_pending().unwrap().iter().last().unwrap().frame, GestureFrame::Released(distance) if (distance-240.0).abs()<0.01)
    );
    bridge.set_revealed(true);
    bridge.responder.sample_contacts(contacts(0.3), false);
    bridge.responder.sample_contacts(contacts(0.5), false);
    assert!(
        matches!(frames.take_pending().unwrap().iter().last().unwrap().frame, GestureFrame::Tracking(distance) if distance<0.0)
    );
    bridge.cancel();
    assert_eq!(
        frames.take_pending().unwrap().iter().last().unwrap().frame,
        GestureFrame::Cancelled
    );
    let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        objc2_app_kit::NSEventType::ApplicationDefined,
        objc2_foundation::NSPoint::new(0.0, 0.0),
        objc2_app_kit::NSEventModifierFlags::empty(),
        0.0,
        0,
        None,
        0,
        0,
        0,
    ).unwrap();
    unsafe {
        let _: () = msg_send![&*view,touchesCancelledWithEvent:&*event];
    }
    assert_eq!(
        frames.take_pending().unwrap().iter().last().unwrap().frame,
        GestureFrame::Cancelled
    );
    assert_eq!(
        previous.ivars().get(),
        1,
        "native NSView touch cancellation must reach and pass through Diri's responder"
    );
    // A component inserted after installation retains its responder and mask.
    let inserted = NSResponder::new(marker);
    unsafe {
        inserted.setNextResponder(Some(&bridge.responder));
        view.setNextResponder(Some(&inserted));
    }
    let changed_mask = if view.allowedTouchTypes() == NSTouchTypeMask::Indirect {
        NSTouchTypeMask::Indirect | NSTouchTypeMask::Direct
    } else {
        NSTouchTypeMask::Indirect
    };
    view.setAllowedTouchTypes(changed_mask);
    drop(bridge);
    assert!(std::ptr::eq(
        unsafe { view.nextResponder() }.as_deref().unwrap(),
        inserted.as_ref()
    ));
    assert!(std::ptr::eq(
        unsafe { inserted.nextResponder() }.as_deref().unwrap(),
        previous.as_super()
    ));
    assert_eq!(view.allowedTouchTypes(), changed_mask);
    // Unmodified ownership restores the exact original chain and allowed types.
    view.setAllowedTouchTypes(original_mask);
    let (bridge, _) = TabGestureBridge::install_view(marker, view.clone());
    let inserted_below = NSResponder::new(marker);
    unsafe {
        inserted_below.setNextResponder(Some(&inserted));
        bridge.responder.setNextResponder(Some(&inserted_below));
    }
    drop(bridge);
    assert_eq!(view.allowedTouchTypes(), original_mask);
    assert!(std::ptr::eq(
        unsafe { view.nextResponder() }.as_deref().unwrap(),
        inserted_below.as_ref()
    ));
}

#[cfg(test)]
fn verify_coalesced_stroke_boundaries(marker: MainThreadMarker) {
    use crate::tab_peek::TabPeek;
    let (bridge, mut frames) = TabGestureBridge::install_view(marker, NSView::new(marker));
    bridge.set_revealed(true);
    let contacts = |y| (1..=3).map(|id| (id, 0.5, y)).collect();
    let now = std::time::Instant::now();
    let mut peek = TabPeek::default();
    peek.begin(vec!["one"], Some(&"one"));
    peek.animate_to(380.0, now, true);
    bridge.responder.sample_contacts(contacts(0.3), false);
    bridge
        .responder
        .sample_contacts(contacts(0.3 + 200.0 / 1200.0), false);
    for sample in frames.take_pending().unwrap().iter() {
        peek.update_animated(sample.frame, sample.observed_at, true);
    }
    assert!((peek.overview() - 40.0 / 240.0).abs() < 0.001);
    // AppKit can deliver a release and another stroke before the async GPUI
    // consumer runs. The release must settle this first stroke to peek before
    // the second stroke takes its origin.
    bridge.responder.sample_contacts(Vec::new(), false);
    bridge.responder.sample_contacts(contacts(0.3), false);
    bridge.responder.sample_contacts(contacts(0.35), false);
    for sample in frames.take_pending().unwrap().iter() {
        peek.update_animated(sample.frame, sample.observed_at, true);
    }
    assert!(
        (peek.reveal() - 80.0 / 140.0).abs() < 0.001,
        "a delayed consumer lost the stroke boundary: reveal={}, overview={}",
        peek.reveal(),
        peek.overview()
    );
}

#[cfg(test)]
fn verify_overflow_waits_for_lift(marker: MainThreadMarker) {
    use crate::tab_peek::TabPeek;
    let (bridge, mut frames) = TabGestureBridge::install_view(marker, NSView::new(marker));
    let contacts = |y| (1..=3).map(|id| (id, 0.5, y)).collect();
    for _ in 0..8 {
        bridge.responder.sample_contacts(contacts(0.7), false);
        bridge.responder.sample_contacts(contacts(0.6), false);
        bridge.responder.sample_contacts(Vec::new(), false);
    }
    bridge.responder.sample_contacts(contacts(0.7), false);
    bridge.responder.sample_contacts(contacts(0.5), false);
    assert!(bridge.responder.ivars().suppress_until_lift.get());
    let now = std::time::Instant::now();
    let mut peek = TabPeek::default();
    peek.begin(vec!["one"], None);
    peek.animate_to(380.0, now, true);
    for sample in frames.take_pending().unwrap().iter() {
        assert_eq!(sample.frame, GestureFrame::Cancelled);
        peek.update_animated(sample.frame, sample.observed_at, true);
    }
    assert!(!peek.visible());
    assert!(!peek.tracking);
    // Capacity is available again, but these fingers still belong to the
    // discarded stroke. They must not reopen the cancelled presentation.
    bridge.responder.sample_contacts(contacts(0.4), false);
    bridge.responder.sample_contacts(contacts(0.3), false);
    assert!(frames.take_pending().is_none());
    bridge.responder.sample_contacts(Vec::new(), false);
    assert!(!bridge.responder.ivars().suppress_until_lift.get());
    bridge.responder.sample_contacts(contacts(0.7), false);
    bridge.responder.sample_contacts(contacts(0.6), false);
    bridge.responder.sample_contacts(Vec::new(), false);
    peek.begin(vec!["one"], None);
    for sample in frames.take_pending().unwrap().iter() {
        peek.update_animated(sample.frame, sample.observed_at, true);
    }
    assert_eq!(peek.reveal(), 1.0);
    assert_eq!(peek.overview(), 0.0);
    assert!(!peek.tracking);
    // Cancelling an idle window must not swallow the next complete gesture.
    bridge.cancel();
    frames.take_pending().unwrap();
    bridge.responder.sample_contacts(contacts(0.7), false);
    bridge.responder.sample_contacts(contacts(0.6), false);
    bridge.responder.sample_contacts(Vec::new(), false);
    assert_eq!(frames.take_pending().unwrap().iter().count(), 2);
}
