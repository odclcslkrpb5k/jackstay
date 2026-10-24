//! The selection engine, driven through a fake X server.

/// Builds the event first, so it may use the fake server, then hands it to the engine.
macro_rules! send {
    ($e:expr, $x:expr, $ev:expr, $now:expr $(,)?) => {{
        let ev = $ev;
        $e.handle($x, ev, $now)
    }};
}

mod edges;
mod liveness;
mod races;
mod scenarios;
mod support;
