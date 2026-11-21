//! Identify transient transport failures before mapping to local, sanitized API errors.

use std::{error::Error, io};

pub(crate) fn is_transient_status(status: u16) -> bool {
    // Rate limiting and permanent HTTP failures are not retried on this short budget.
    matches!(status, 408 | 500 | 502..=504)
}

pub(crate) fn is_transient_error(error: &reqwest::Error) -> bool {
    if error.is_timeout() {
        return true;
    }
    // Read the typed cause only. TLS validation and malformed HTTP are not
    // transient, while a peer closing before a complete HTTP message can be.
    let mut cause = error.source();
    while let Some(source) = cause {
        if source
            .downcast_ref::<hyper::Error>()
            .is_some_and(hyper::Error::is_incomplete_message)
            || source.downcast_ref::<io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::NetworkUnreachable
                        | io::ErrorKind::HostUnreachable
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::TimedOut
                )
            })
        {
            return true;
        }
        cause = source.source();
    }
    false
}
