use log::error;
use mio::{Event, Events, Poll};

pub trait EventHandler {
    /// Returns a reference to the handler's `Poll`.
    fn poll(&self) -> &Poll;

    /// Returns `true` to stop the event loop.
    fn handle(&mut self, event: Event) -> bool;
}

pub fn run_event_loop<T: EventHandler>(handler: &mut T) {
    let mut events = Events::with_capacity(1024);
    'event_loop: loop {
        if let Err(err) = handler.poll().poll(&mut events, None) {
            error!("error when polling event loop: {}", err);
            // These are probably unrecoverable.
            break;
        }

        for event in events.iter() {
            if handler.handle(event) {
                break 'event_loop;
            }
        }
    }
}
