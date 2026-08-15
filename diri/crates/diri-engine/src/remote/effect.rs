//! At-most-once writes for remote side effects.
//!
//! A failed write before any byte was accepted is safe to retry. Once even one
//! byte was accepted, the Holder may have applied the complete frame while the
//! acknowledgement path failed; replaying it could duplicate input or a
//! signal. This module makes that distinction explicit and testable.

use std::fmt;
use std::io::{self, Write};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EffectOutcome {
    NotApplied,
    Unknown,
}

#[derive(Debug)]
pub(crate) struct EffectWriteError {
    outcome: EffectOutcome,
    source: io::Error,
}

impl EffectWriteError {
    pub(crate) fn not_applied(source: io::Error) -> Self {
        Self {
            outcome: EffectOutcome::NotApplied,
            source,
        }
    }

    pub(crate) fn outcome(&self) -> EffectOutcome {
        self.outcome
    }
}

impl fmt::Display for EffectWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.outcome {
            EffectOutcome::NotApplied => {
                write!(formatter, "remote effect was not applied: {}", self.source)
            }
            EffectOutcome::Unknown => write!(
                formatter,
                "remote effect outcome is unknown and must not be retried: {}",
                self.source
            ),
        }
    }
}

impl std::error::Error for EffectWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<EffectWriteError> for io::Error {
    fn from(error: EffectWriteError) -> Self {
        io::Error::other(error)
    }
}

pub(crate) fn write_at_most_once(
    writer: &mut impl Write,
    bytes: &[u8],
) -> Result<(), EffectWriteError> {
    let mut written = 0;
    while written < bytes.len() {
        match writer.write(&bytes[written..]) {
            Ok(0) => {
                return Err(classify(
                    written,
                    io::Error::new(
                        io::ErrorKind::WriteZero,
                        "remote effect write returned zero",
                    ),
                ));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(classify(written, error)),
        }
    }
    writer.flush().map_err(|error| EffectWriteError {
        // All bytes entered the transport, so the Holder may have applied them.
        outcome: EffectOutcome::Unknown,
        source: error,
    })
}

fn classify(written: usize, source: io::Error) -> EffectWriteError {
    EffectWriteError {
        outcome: if written == 0 {
            EffectOutcome::NotApplied
        } else {
            EffectOutcome::Unknown
        },
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWriter {
        accept: usize,
        accepted: usize,
        fail_flush: bool,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.accepted >= self.accept {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "disconnected"));
            }
            let count = bytes.len().min(self.accept - self.accepted);
            self.accepted += count;
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "lost receipt"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn failure_before_the_first_byte_is_safe_to_retry() {
        let error = write_at_most_once(
            &mut FailingWriter {
                accept: 0,
                accepted: 0,
                fail_flush: false,
            },
            b"effect",
        )
        .expect_err("write fails");
        assert_eq!(error.outcome(), EffectOutcome::NotApplied);
    }

    #[test]
    fn partial_write_has_an_unknown_outcome() {
        let error = write_at_most_once(
            &mut FailingWriter {
                accept: 2,
                accepted: 0,
                fail_flush: false,
            },
            b"effect",
        )
        .expect_err("write fails");
        assert_eq!(error.outcome(), EffectOutcome::Unknown);
    }

    #[test]
    fn a_lost_flush_receipt_has_an_unknown_outcome() {
        let error = write_at_most_once(
            &mut FailingWriter {
                accept: usize::MAX,
                accepted: 0,
                fail_flush: true,
            },
            b"effect",
        )
        .expect_err("flush fails");
        assert_eq!(error.outcome(), EffectOutcome::Unknown);
    }
}
