//! A consumer implementation which produces messages from a given input stream.
//!
//! This can be useful for testing

use futures_util::{pin_mut, stream::{self, Stream, BoxStream, StreamExt}};
use futures_channel::mpsc;
use async_trait::async_trait;
use crate::{ValidatedMessage, consume::AcknowledgeableMessage};
use std::{pin::Pin, time::Duration};

/// A consumer implementation which yields messages from the user's given input stream, and
/// re-delivers messages if they are not acknowledged within the timeout deadline.
pub struct MockConsumer<E> {
    stream: Pin<Box<dyn Stream<Item=Result<AcknowledgeableMessage<AckToken, ValidatedMessage>, E>>>>,
}

impl<E> MockConsumer<E> {
    /// Create a new MockConsumer which will yield messages from the given input stream.
    ///
    /// Errors from the input stream will be forwarded on the output stream; this can be useful for
    /// simulating stream failures.
    ///
    /// Messages will be re-delivered if not acknowledged within the timeout deadline. The default
    /// deadline for messages, in seconds, is given as an argument to this function. This deadline
    /// can be modified using the returned `AckToken`s, however those modifications will only be in
    /// effect for that message's next delivery; subsequent deliveries will revert to the default
    /// deadline.
    pub fn new<S>(input_stream: S, ack_deadline_seconds: u32) -> MockConsumer<E>
        where S: Stream<Item=Result<ValidatedMessage, E>>,
              E: Unpin
    {
        let stream = Box::pin(async_stream::stream! {
            // Each incoming message will be turned into a repeating stream of that message,
            // together with ack tokens to stop (or modify) that stream's repetition. The outer
            // mock consumer is essentially a select_all over these repeating streams, and
            // additionally adds new messages from the user's given inputs

            let repeating_messages = stream::SelectAll::new();
            pin_mut!(repeating_messages);

            let input_stream = input_stream.fuse();
            pin_mut!(input_stream);

            loop {
                yield futures_util::select! {
                    new_input = input_stream.select_next_some() => {
                        match new_input {
                            Err(e) => Err(e),
                            Ok(msg) => {
                                repeating_messages.push(Self::repeat(msg, ack_deadline_seconds));
                                continue;
                            }
                        }
                    },
                    (ack_token, message) = repeating_messages.select_next_some() => {
                        Ok(AcknowledgeableMessage { ack_token, message })
                    },

                    // if both the input stream and the repeating streams are terminated, the
                    // output stream is done too
                    complete => return,
                };
            }
        });

        Self {
            stream
        }
    }

    /// Create a stream which will repeatedly produce the given message unless the returned ack
    /// tokens are acknowledged within the given timeout.
    ///
    /// If any of the returned ack tokens is ever acknowledged, the stream will stop repeating and
    /// terminate. The ack tokens can also be used to repeat a message sooner; however only the
    /// next repeated instance will be delivered on the modified deadline, and subsequent ones will
    /// repeat on the base interval.
    fn repeat(message: ValidatedMessage, base_timeout_seconds: u32) -> impl Stream<Item = (AckToken, ValidatedMessage)> + Unpin {
        Box::pin(async_stream::stream! {
            let base_timeout = Duration::from_secs(u64::from(base_timeout_seconds));
            let (sender, mut receiver) = mpsc::unbounded();

            'send_message: loop {
                yield (AckToken { channel: sender.clone() }, message.clone());

                // every time the message is sent, the timeout is reset to the base.
                // This prevents time modification from sticking to all subsequent deliveries --
                // otherwise nacks would trigger floods of messages
                let mut timeout = base_timeout;
                'wait_timeout: loop {
                    match tokio::time::timeout(timeout, receiver.next()).await {
                        Ok(Some(UserAck::Ack)) => {
                            // got an ack, stop repeating
                            return;
                        },
                        Ok(Some(UserAck::Modify { seconds })) => {
                            // user wants to change the timeout before another send
                            timeout = Duration::from_secs(u64::from(seconds));
                            continue 'wait_timeout;
                        },
                        Ok(None) => unreachable!("senders never close, receiver should never terminate"),
                        Err(tokio::time::error::Elapsed { .. }) => {
                            // no ack in time, repeat the message
                            continue 'send_message;
                        }
                    }
                }
            }
        })
    }
}

impl<E> crate::consume::Consumer for MockConsumer<E>
where E: std::error::Error + Send + Sync + 'static
{
    type AckToken = AckToken;
    type Error = E;
    type Stream = BoxStream<'static, Result<AcknowledgeableMessage<Self::AckToken, ValidatedMessage>, Self::Error>>;

    fn stream(self) -> Self::Stream {
        self.stream
    }
}

/// TODO
pub struct AckToken {
    channel: mpsc::UnboundedSender<UserAck>,
}

/// TODO
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
#[error("could not send ack/nack/modify because the stream has been dropped")]
pub struct AckError {
    _private: (),
}

impl From<mpsc::TrySendError<UserAck>> for AckError {
    fn from(from: mpsc::TrySendError<UserAck>) -> Self {
        assert!(from.is_disconnected(), "send should not fail unless the channel disconnected");

        AckError {
            _private: ()
        }
    }
}

#[async_trait]
impl crate::consume::AcknowledgeToken for AckToken {
    type AckError = AckError;
    type NackError = AckError;
    type ModifyError = AckError;

    async fn ack(self) -> Result<(), Self::AckError> {
        self.channel.unbounded_send(UserAck::Ack)?;
        Ok(())
    }

    async fn nack(mut self) -> Result<(), Self::NackError> {
        self.modify_deadline(0).await
    }

    async fn modify_deadline(&mut self, seconds: u32) -> Result<(), Self::ModifyError> {
        self.channel.unbounded_send(UserAck::Modify { seconds })?;
        Ok(())
    }

}

enum UserAck {
    Ack,
    Modify { seconds: u32 },
}
