// One invocation owns its Store and supervises guest and outgoing work.

impl Invocation {
    pub(crate) fn new(
        lib: Arc<Lib>,
        permit: tokio::sync::OwnedSemaphorePermit,
        endpoint: dhttp::Endpoint,
        handshake: &dhttp::HandshakeSummary,
        tasks: TaskTracker,
    ) -> Result<Self> {
        let local = handshake.local.as_ref().ok_or(Error::MissingHandshake)?;
        if local.name() != endpoint.name() {
            return Err(Error::IdentityMismatch);
        }
        if lib.cancel.is_cancelled() || tasks.is_closed() {
            return Err(Error::Cancelled);
        }
        let producer_cancel = lib.cancel.child_token();
        Ok(Self {
            lib,
            local: local.clone(),
            remote: handshake.remote.clone(),
            endpoint,
            producer_cancel,
            permit,
            tasks,
        })
    }

    pub(crate) async fn execute(self, request: Request<Body>) -> Result<Response<Body>> {
        let Self {
            lib,
            local,
            remote,
            endpoint,
            producer_cancel,
            permit,
            tasks,
        } = self;
        let cancel_on_drop = producer_cancel.clone().drop_guard();
        if producer_cancel.is_cancelled() || tasks.is_closed() {
            return Err(Error::Cancelled);
        }
        let scheme = match request.uri().scheme_str() {
            Some("http") => types::Scheme::Http,
            Some("https") => types::Scheme::Https,
            _ => {
                return Err(Error::BadRequest(
                    "Lib request requires an HTTP scheme".into(),
                ));
            }
        };
        let children = TaskTracker::new();
        let outgoing_cancel = producer_cancel.child_token();
        let outgoing = HostOutgoing {
            endpoint: remote
                .as_ref()
                .filter(|caller| caller.name() == local.name())
                .map(|_| endpoint),
            policy: copy_policy(&lib.policy),
            remaining_requests: 16,
            children: children.clone(),
            cancel: outgoing_cancel.clone(),
        };
        let mut wasi = WasiCtx::builder().build();
        *wasi.filesystem() = lib.filesystem.clone();
        let store_data = StoreData {
            table: ResourceTable::new(),
            wasi,
            http: WasiHttpCtx::new(),
            memory: MemoryLimits {
                base: StoreLimitsBuilder::new()
                    .memory_size(64 << 20)
                    .instances(32)
                    .memories(32)
                    .tables(64)
                    .table_elements(100_000)
                    .build(),
                used: 0,
                pending: 0,
            },
            outgoing,
            local,
            remote,
            policy: copy_policy(&lib.policy),
            permit,
        };
        let request = request.map(|body| {
            body.map_err(|error| types::ErrorCode::InternalError(Some(error.to_string())))
        });
        let (response_tx, mut response_rx) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        // The supervisor owns and reaps the actual guest task even if nobody
        // polls the HTTP response body again after receiving its headers.
        let mut supervisor = tasks.spawn(async move {
            let mut guest = tokio::spawn(async move {
                let mut store = Store::new(&lib.runtime.engine, store_data);
                store.limiter(|state| &mut state.memory);
                store.set_fuel(100_000_000).map_err(Error::Guest)?;
                store
                    .fuel_async_yield_interval(Some(10_000))
                    .map_err(Error::Guest)?;
                let proxy =
                    Proxy::instantiate_async(&mut store, &lib.component, &lib.runtime.linker)
                        .await
                        .map_err(Error::Guest)?;
                let incoming = store
                    .data_mut()
                    .http()
                    .new_incoming_request(scheme, request)
                    .map_err(Error::Guest)?;
                let outparam = store
                    .data_mut()
                    .http()
                    .new_response_outparam(response_tx)
                    .map_err(Error::Guest)?;
                proxy
                    .wasi_http_incoming_handler()
                    .call_handle(&mut store, incoming, outparam)
                    .await
                    .map_err(Error::Guest)
            });
            let outcome = tokio::select! {
                biased;
                _ = producer_cancel.cancelled() => {
                    guest.abort();
                    let _ = guest.await;
                    Err(Error::Cancelled)
                }
                _ = tokio::time::sleep_until(deadline) => {
                    producer_cancel.cancel();
                    guest.abort();
                    let _ = guest.await;
                    Err(Error::Deadline)
                }
                outcome = &mut guest => outcome.map_err(Error::Task).and_then(|outcome| outcome),
            };
            outgoing_cancel.cancel();
            children.close();
            children.wait().await;
            outcome
        });

        let (response, guest) = tokio::select! {
            biased;
            // Read the result, rather than is_finished(), so an error cannot
            // race a successful response-head submission unnoticed.
            outcome = &mut supervisor => {
                outcome.map_err(Error::Task)??;
                let response = response_rx.try_recv().map_err(|_| Error::GuestExitedWithoutResponse)?
                    .map_err(Error::GuestRejectedResponse)?;
                (response, None)
            }
            response = &mut response_rx => {
                let response = match response {
                    Ok(response) => response.map_err(Error::GuestRejectedResponse)?,
                    Err(_) => {
                        supervisor.await.map_err(Error::Task)??;
                        return Err(Error::GuestExitedWithoutResponse);
                    }
                };
                // If both became ready during this poll, consume the task
                // result now. Pending leaves its wake registration intact.
                match futures::poll!(&mut supervisor) {
                    Poll::Ready(outcome) => { outcome.map_err(Error::Task)??; (response, None) }
                    Poll::Pending => (response, Some(supervisor)),
                }
            }
        };
        let (parts, inner) = response.into_parts();
        Ok(Response::from_parts(
            parts,
            LibResponseBody::Reading {
                inner,
                guest,
                cancel_on_drop,
            }
            .map_err(Error::body_error)
            .boxed_unsync(),
        ))
    }
}
