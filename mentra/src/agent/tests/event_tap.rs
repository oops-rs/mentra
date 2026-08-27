use std::{
    sync::{
        Arc, Barrier, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use super::super::{AgentEvent, AgentEventBus};

fn event(label: impl Into<String>) -> AgentEvent {
    let label = label.into();
    AgentEvent::TextDelta {
        delta: label.clone(),
        full_text: label,
    }
}

fn label(event: AgentEvent) -> String {
    match event {
        AgentEvent::TextDelta { delta, .. } => delta,
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn concurrent_senders_are_serialized_in_the_same_order_as_broadcast() {
    const SENDERS: usize = 16;

    let bus = AgentEventBus::new(SENDERS * 2);
    let mut receiver = bus.subscribe();
    let active = Arc::new(AtomicUsize::new(0));
    let overlapped = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let active_for_tap = Arc::clone(&active);
    let overlapped_for_tap = Arc::clone(&overlapped);
    let observed_for_tap = Arc::clone(&observed);
    let _guard = bus.register_tap(move |event| {
        if active_for_tap.fetch_add(1, Ordering::SeqCst) != 0 {
            overlapped_for_tap.store(true, Ordering::SeqCst);
        }
        thread::sleep(Duration::from_millis(2));
        observed_for_tap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(label(event.clone()));
        active_for_tap.fetch_sub(1, Ordering::SeqCst);
    });

    let start = Arc::new(Barrier::new(SENDERS));
    thread::scope(|scope| {
        for index in 0..SENDERS {
            let bus = bus.clone();
            let start = Arc::clone(&start);
            scope.spawn(move || {
                start.wait();
                bus.send(event(index.to_string()));
            });
        }
    });

    assert!(!overlapped.load(Ordering::SeqCst));
    let observed = observed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let broadcast = (0..SENDERS)
        .map(|_| label(receiver.try_recv().expect("broadcast event")))
        .collect::<Vec<_>>();
    assert_eq!(observed, broadcast);
}

#[test]
fn dropping_a_guard_waits_for_an_in_flight_callback() {
    let bus = AgentEventBus::new(8);
    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let entered_for_tap = Arc::clone(&entered);
    let release_for_tap = Arc::clone(&release);
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_tap = Arc::clone(&calls);
    let guard = bus.register_tap(move |_| {
        calls_for_tap.fetch_add(1, Ordering::SeqCst);
        let (entered, wake) = &*entered_for_tap;
        *entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        wake.notify_all();

        let (released, wake) = &*release_for_tap;
        let released = released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(
            wake.wait_while(released, |released| !*released)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    });

    let sender_bus = bus.clone();
    let sender = thread::spawn(move || sender_bus.send(event("first")));
    let (entered_lock, entered_wake) = &*entered;
    let entered_guard = entered_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    drop(
        entered_wake
            .wait_while(entered_guard, |entered| !*entered)
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );

    let (started_tx, started_rx) = mpsc::channel();
    let (dropped_tx, dropped_rx) = mpsc::channel();
    let dropper = thread::spawn(move || {
        started_tx.send(()).expect("report drop attempt");
        drop(guard);
        dropped_tx.send(()).expect("report guard drop");
    });
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("dropper reached guard drop");
    let dropped_while_callback_was_running =
        dropped_rx.recv_timeout(Duration::from_millis(50)).is_ok();

    let (released, release_wake) = &*release;
    *released
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    release_wake.notify_all();

    if !dropped_while_callback_was_running {
        dropped_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("guard drop completes after callback");
    }
    sender.join().expect("sender exits");
    dropper.join().expect("dropper exits");

    assert!(!dropped_while_callback_was_running);

    bus.send(event("second"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn callback_captures_are_destroyed_after_dispatch_locks_are_released() {
    let bus = AgentEventBus::new(8);
    let inner = bus.register_tap(|_| {});
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_tap = Arc::clone(&calls);
    let outer = bus.register_tap(move |_| {
        let _keep_inner_registered = &inner;
        calls_for_tap.fetch_add(1, Ordering::SeqCst);
    });

    let (dropped_tx, dropped_rx) = mpsc::channel();
    let dropper = thread::spawn(move || {
        drop(outer);
        dropped_tx.send(()).expect("report nested guard drop");
    });
    dropped_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("captured guard destructor must not recurse under dispatch locks");
    dropper.join().expect("dropper exits");

    bus.send(event("after-drop"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
