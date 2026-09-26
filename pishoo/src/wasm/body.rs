// Response frames retain cancellation ownership until normal completion.

impl HttpBody for LibResponseBody {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>>>> {
        let this = self.get_mut();
        loop {
            match this {
                Self::Ended => return Poll::Ready(None),
                Self::Reading { inner, guest, .. } => {
                    if let Some(task) = guest {
                        if let Poll::Ready(outcome) = Pin::new(task).poll(cx) {
                            *guest = None;
                            if let Err(error) =
                                outcome.map_err(Error::Task).and_then(|outcome| outcome)
                            {
                                *this = Self::Ended;
                                return Poll::Ready(Some(Err(error)));
                            }
                        }
                    }
                    let trailers = match Pin::new(inner).poll_frame(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Some(Ok(frame))) if frame.is_data() => {
                            return Poll::Ready(Some(Ok(frame)));
                        }
                        Poll::Ready(Some(Ok(frame))) => frame.into_trailers().ok(),
                        Poll::Ready(Some(Err(error))) => {
                            *this = Self::Ended;
                            return Poll::Ready(Some(Err(Error::GuestRejectedResponse(error))));
                        }
                        Poll::Ready(None) => None,
                    };
                    let Self::Reading {
                        guest,
                        cancel_on_drop,
                        ..
                    } = std::mem::replace(this, Self::Ended)
                    else {
                        unreachable!()
                    };
                    match guest {
                        Some(guest) => {
                            *this = Self::Waiting {
                                guest,
                                trailers,
                                cancel_on_drop,
                            }
                        }
                        None => {
                            cancel_on_drop.disarm();
                            return Poll::Ready(
                                trailers.map(|headers| Ok(Frame::trailers(headers))),
                            );
                        }
                    }
                }
                Self::Waiting { guest, .. } => {
                    let outcome = match Pin::new(guest).poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(outcome) => {
                            outcome.map_err(Error::Task).and_then(|outcome| outcome)
                        }
                    };
                    let Self::Waiting {
                        trailers,
                        cancel_on_drop,
                        ..
                    } = std::mem::replace(this, Self::Ended)
                    else {
                        unreachable!()
                    };
                    match outcome {
                        Ok(()) => {
                            cancel_on_drop.disarm();
                            return Poll::Ready(
                                trailers.map(|headers| Ok(Frame::trailers(headers))),
                            );
                        }
                        Err(error) => return Poll::Ready(Some(Err(error))),
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, Self::Ended)
    }
}
