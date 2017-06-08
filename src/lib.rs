//! Allow signal-based termination to be handled in a more graceful manner.
//!
//! ```
//! extern crate futures;
//! extern crate tokio_core;
//! extern crate tokio_signal_catcher;
//!
//! fn main() {
//!     { // everything within this scope will be dropped gracefully
//!       // when a signal is received
//!         let mut core = tokio_core::reactor::Core::new().unwrap();
//!         let handle = core.handle();
//!         core.run(tokio_signal_catcher::catch(&handle, {
//!             // your main future goes here
//!             // ...
//! #           futures::future::ok(())
//!         }))
//!     }.unwrap_or_else(|e| e.terminate());
//! }
//! ```

extern crate futures;
extern crate libc;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_signal;
extern crate void;

#[cfg(unix)]
pub mod unix {
    use std::{io, mem, process, ptr};
    use std::os::raw;
    use libc;
    use tokio_core::reactor::Handle;
    use tokio_io::IoFuture;
    use tokio_signal;

    pub type Signal = raw::c_int;

    pub const DEFAULT_SIGNALS: [Signal; 3] = [
        tokio_signal::unix::SIGHUP,
        tokio_signal::unix::SIGINT,
        tokio_signal::unix::SIGTERM,
    ];

    pub fn terminate_with(signal: Signal) -> ! {
        unsafe {
            let mut sa: libc::sigaction = mem::zeroed();
            sa.sa_sigaction = libc::SIG_DFL;
            if libc::sigemptyset(&mut sa.sa_mask) != 0 {
                panic!("sigemptyset failed: {}", io::Error::last_os_error());
            }
            if libc::sigaction(signal, &sa, ptr::null_mut()) != 0 {
                panic!("sigaction failed: {}", io::Error::last_os_error());
            }
            libc::kill(libc::getpid(), signal);
        }
        process::exit(1);
    }

    pub type SignalStream = tokio_signal::unix::Signal;

    pub fn signal_stream(signal: Signal, handle: &Handle)
                         -> IoFuture<SignalStream> {
        SignalStream::new(signal, handle)
    }
}

#[cfg(windows)]
pub mod windows {
    use std::{io, process};
    use futures::{Future, Poll, Stream};
    use tokio_core::reactor::Handle;
    use tokio_io::IoFuture;
    use tokio_signal;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub enum Signal {
        CtrlBreak,
        CtrlC,
        //CtrlClose, // FIXME: not yet implemented by tokio-signals
        #[doc(hidden)]
        __Nonexhaustive,
    }

    pub const DEFAULT_SIGNALS: [Signal; 2] = [
        Signal::CtrlBreak,
        Signal::CtrlC,
    ];

    pub fn terminate_with(signal: Signal) -> ! {
        let _unused = signal;
        const STATUS_CONTROL_C_EXIT: i32 = -0x3ffffec6;
        process::exit(STATUS_CONTROL_C_EXIT);
    }

    pub struct SignalStream {
        pub signal: Signal,
        pub stream: tokio_signal::windows::Event,
    }

    impl Stream for SignalStream {
        type Item = Signal;
        type Error = io::Error;
        fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
            self.stream.poll().map(|x| x.map(|x| x.map(|()| self.signal)))
        }
    }

    pub fn signal_stream(signal: Signal, handle: &Handle)
                         -> IoFuture<SignalStream> {
        match signal {
            Signal::CtrlBreak =>
                tokio_signal::windows::Event::ctrl_break(handle),
            Signal::CtrlC =>
                tokio_signal::windows::Event::ctrl_c(handle),
            _ => panic!("invalid signal: {:?}", signal),
        }.map(move |stream| SignalStream { signal, stream }).boxed()
    }
}

use std::{io, mem};
use futures::{Async, Future, Poll, Stream};
use futures::future::{self, Either, JoinAll, Select2, SelectAll};
use futures::stream::StreamFuture;
use tokio_core::reactor::Handle;
use tokio_io::IoFuture;
use void::Void;
#[cfg(unix)]
use self::unix as native;
#[cfg(windows)]
use self::windows as native;

/// Represents a signal value.  For platform-independent applications, these
/// should be treated as opaque objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Signal(pub native::Signal);

impl Signal {
    /// Use the signal to terminate in a platform-dependent manner.
    ///
    /// When a signal is caught, it is recommended to use this function to
    /// terminate the program so that the parent process is aware of what
    /// caused the termination.
    ///
    /// On Unix platforms, this will reset the signal handler and kill the
    /// current process with given signal.  If the default behavior of the
    /// signal is not termination, the program will instead exit with a
    /// nonzero status.
    ///
    /// On Windows, this will exit with `STATUS_CONTROL_C_EXIT`
    /// (`0xc000013a`).
    pub fn terminate(self) -> ! {
        native::terminate_with(self.0)
    }

    /// Obtain a stream associated with this signal.
    pub fn stream(self, handle: &Handle) -> IoFuture<SignalStream> {
        native::signal_stream(self.0, handle).map(SignalStream).boxed()
    }

    /// The signals caught by `catch_signals`.
    pub fn defaults() -> Vec<Signal> {
        native::DEFAULT_SIGNALS.iter().map(|&sig| {
            Signal(sig)
        }).collect()
    }
}

/// A stream of signals, which can be created using `Signa::stream`.
#[must_use = "streams do nothing unless polled"]
pub struct SignalStream(pub native::SignalStream);

impl Stream for SignalStream {
    type Item = Signal;
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.poll().map(|x| x.map(|x| x.map(Signal)))
    }
}

enum CatchState<F> {
    Starting(JoinAll<Vec<IoFuture<SignalStream>>>, F),
    Running(Select2<F, SelectAll<StreamFuture<SignalStream>>>),
    Invalid,
}

#[must_use = "futures do nothing unless polled"]
pub struct Catch<F>(CatchState<F>);

impl<F: Future<Error=Void>> Future for Catch<F> {
    type Item = F::Item;
    type Error = Signal;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let streams = match self.0 {
                CatchState::Starting(ref mut s, _) => match s.poll() {
                    Err(e) =>
                        panic!("could not register signal handler: {}", e),
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(streams)) => streams,
                },
                CatchState::Running(ref mut f) => match f.poll() {
                    Err(Either::A((e, _))) => match e {},
                    Err(Either::B((((e, _), _, _), _))) =>
                        panic!("signal stream errored: {}", e),
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(Either::A((x, _)))) =>
                        return Ok(Async::Ready(x)),
                    Ok(Async::Ready(Either::B((((None, _), _, _), _)))) =>
                        panic!("signal stream stopped unexpectedly"),
                    Ok(Async::Ready(Either::B((((Some(sig), _), _, _), _)))) =>
                        return Err(sig),
                },
                _ => panic!("future is no longer pollable"),
            };
            match mem::replace(&mut self.0, CatchState::Invalid) {
                CatchState::Starting(_, f) => {
                    self.0 = CatchState::Running(
                        f.select2(future::select_all(
                            streams.into_iter().map(|s| s.into_future()))));
                }
                _ => unreachable!(),
            }
        }
    }
}

/// Signals that occur during the execution of the given future will cause the
/// future to exit gracefully with `Signal` as the error.
///
/// ```ignore
/// fn catch(Future<T, !>) -> Future<T, Signal>;
/// ```
pub fn catch<F: Future<Error=Void>>(handle: &Handle, f: F) -> Catch<F> {
    Catch(CatchState::Starting(
        future::join_all(Signal::defaults().into_iter().map(|sig| {
            sig.stream(handle)
        }).collect()),
        f,
    ))
}
