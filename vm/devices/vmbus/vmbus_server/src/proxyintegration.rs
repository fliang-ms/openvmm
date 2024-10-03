// Copyright (C) Microsoft Corporation. All rights reserved.

//! Implements support for using kernel-mode VMBus channel provider (VSPs) via
//! the vmbusproxy driver.

#![cfg(windows)]

use super::ChannelRequest;
use super::Guid;
use super::OfferInfo;
use super::OfferRequest;
use super::ProxyHandle;
use super::TaggedStream;
use super::VmbusServerControl;
use anyhow::Context;
use futures::stream::SelectAll;
use futures::StreamExt;
use guestmem::GuestMemory;
use mesh::error::RemoteResultExt;
use mesh::rpc::Rpc;
use pal_async::driver::SpawnDriver;
use pal_async::task::Spawn;
use pal_event::Event;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::os::windows::prelude::*;
use std::sync::Arc;
use vmbus_channel::bus::ChannelServerRequest;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::bus::OpenRequest;
use vmbus_channel::gpadl::GpadlId;
use vmbus_proxy::vmbusioctl::VMBUS_CHANNEL_ENUMERATE_DEVICE_INTERFACE;
use vmbus_proxy::vmbusioctl::VMBUS_CHANNEL_NAMED_PIPE_MODE;
use vmbus_proxy::vmbusioctl::VMBUS_CHANNEL_REQUEST_MONITORED_NOTIFICATION;
use vmbus_proxy::vmbusioctl::VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS;
use vmbus_proxy::ProxyAction;
use vmbus_proxy::VmbusProxy;
use vmcore::interrupt::Interrupt;
use zerocopy::AsBytes;

pub(crate) async fn start_proxy(
    driver: &(impl SpawnDriver + Clone),
    handle: ProxyHandle,
    server: Arc<VmbusServerControl>,
    mem: &GuestMemory,
) -> io::Result<OwnedHandle> {
    let mut proxy = VmbusProxy::new(driver, handle)?;
    let handle = proxy.handle().try_clone_to_owned()?;
    proxy.set_memory(mem).await?;

    driver
        .spawn("vmbus_proxy", proxy_thread(driver.clone(), proxy, server))
        .detach();

    Ok(handle)
}

struct Channel {
    server_request_send: Option<mesh::Sender<ChannelServerRequest>>,
    worker_result: Option<mesh::OneshotReceiver<()>>,
}

struct ProxyTask {
    channels: Arc<Mutex<HashMap<u64, Channel>>>,
    gpadls: Arc<Mutex<HashMap<u64, HashSet<GpadlId>>>>,
    proxy: Arc<VmbusProxy>,
    server: Arc<VmbusServerControl>,
}

impl ProxyTask {
    fn new(control: Arc<VmbusServerControl>, proxy: Arc<VmbusProxy>) -> Self {
        Self {
            channels: Arc::new(Mutex::new(HashMap::new())),
            gpadls: Arc::new(Mutex::new(HashMap::new())),
            proxy,
            server: control,
        }
    }

    async fn handle_open(&self, proxy_id: u64, open_request: &OpenRequest) -> anyhow::Result<()> {
        let event = open_request
            .interrupt
            .event()
            .context("synic guest event ports not backed by events, not supported")?;

        self.proxy
            .open(
                proxy_id,
                &VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS {
                    RingBufferGpadlHandle: open_request.open_data.ring_gpadl_id.0,
                    DownstreamRingBufferPageOffset: open_request.open_data.ring_offset,
                    NodeNumber: 0, // BUGBUG: NUMA
                },
                event,
            )
            .await
            .context("failed to open channel")?;

        // Start the worker thread for the channel.
        let proxy = Arc::clone(&self.proxy);
        let (send, recv) = mesh::oneshot();
        std::thread::Builder::new()
            .name(format!("vmbus proxy worker {:?}", proxy_id))
            .spawn(move || {
                if let Err(err) = proxy.run_channel(proxy_id) {
                    tracing::error!(err = &err as &dyn std::error::Error, "channel worker error");
                }
                send.send(());
            })
            .unwrap();

        self.channels
            .lock()
            .get_mut(&proxy_id)
            .unwrap()
            .worker_result = Some(recv);

        Ok(())
    }

    async fn handle_close(&self, proxy_id: u64) {
        self.proxy
            .close(proxy_id)
            .await
            .expect("channel close failed");

        // Wait for the worker task.
        let recv = self
            .channels
            .lock()
            .get_mut(&proxy_id)
            .unwrap()
            .worker_result
            .take()
            .expect("channel should be open");

        let _ = recv.await;
    }

    async fn handle_gpadl_create(
        &self,
        proxy_id: u64,
        gpadl_id: GpadlId,
        count: u16,
        buf: &[u64],
    ) -> anyhow::Result<()> {
        self.proxy
            .create_gpadl(proxy_id, gpadl_id.0, count.into(), buf.as_bytes())
            .await
            .context("failed to create gpadl")?;

        self.gpadls
            .lock()
            .entry(proxy_id)
            .or_default()
            .insert(gpadl_id);
        Ok(())
    }

    async fn handle_gpadl_teardown(&self, proxy_id: u64, gpadl_id: GpadlId) {
        assert!(
            self.gpadls
                .lock()
                .get_mut(&proxy_id)
                .unwrap()
                .remove(&gpadl_id),
            "gpadl is registered"
        );

        self.proxy
            .delete_gpadl(proxy_id, gpadl_id.0)
            .await
            .expect("delete gpadl failed");
    }

    async fn handle_offer(
        &self,
        id: u64,
        offer: vmbus_proxy::vmbusioctl::VMBUS_CHANNEL_OFFER,
        incoming_event: Event,
    ) -> Option<mesh::Receiver<ChannelRequest>> {
        let channel_type = if offer.ChannelFlags & VMBUS_CHANNEL_ENUMERATE_DEVICE_INTERFACE != 0 {
            let pipe_mode = u32::from_ne_bytes(offer.UserDefined[..4].try_into().unwrap());
            let message_mode = match pipe_mode {
                vmbus_proxy::vmbusioctl::VMBUS_PIPE_TYPE_BYTE => false,
                vmbus_proxy::vmbusioctl::VMBUS_PIPE_TYPE_MESSAGE => true,
                _ => panic!("BUGBUG: unsupported"),
            };
            ChannelType::Pipe { message_mode }
        } else {
            ChannelType::Device {
                pipe_packets: offer.ChannelFlags & VMBUS_CHANNEL_NAMED_PIPE_MODE != 0,
            }
        };

        let interface_id: Guid = offer.InterfaceType.into();
        let instance_id: Guid = offer.InterfaceInstance.into();

        let offer = OfferParams {
            interface_name: "proxy".to_owned(),
            instance_id,
            interface_id,
            mmio_megabytes: offer.MmioMegabytes,
            mmio_megabytes_optional: offer.MmioMegabytesOptional,
            subchannel_index: offer.SubChannelIndex,
            channel_type,
            use_mnf: (offer.ChannelFlags & VMBUS_CHANNEL_REQUEST_MONITORED_NOTIFICATION) != 0,
            offer_order: id.try_into().ok(),
        };
        let (request_send, request_recv) = mesh::channel();
        let (server_request_send, server_request_recv) = mesh::channel();
        let (send, recv) = mesh::oneshot();
        self.server.send.send(OfferRequest::Offer(
            OfferInfo {
                params: offer.into(),
                event: Interrupt::from_event(incoming_event),
                request_send,
                server_request_recv,
            },
            send,
        ));

        let (request_recv, server_request_send) = match recv.await.flatten() {
            Ok(()) => (Some(request_recv), Some(server_request_send)),
            Err(err) => {
                // Currently there is no way to propagate this failure.
                // FUTURE: consider sending a message back to the control worker to fail the VM operation.
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    interface_id = %interface_id,
                    instance_id = %instance_id,
                    "failed to offer proxy channel"
                );
                (None, None)
            }
        };

        self.channels.lock().insert(
            id,
            Channel {
                server_request_send,
                worker_result: None,
            },
        );
        request_recv
    }

    async fn handle_revoke(&self, id: u64) {
        let response_send = self
            .channels
            .lock()
            .get_mut(&id)
            .unwrap()
            .server_request_send
            .take();

        if let Some(response_send) = response_send {
            drop(response_send);
        } else {
            self.proxy
                .release(id)
                .await
                .expect("vmbus proxy state failure");

            self.channels.lock().remove(&id);
        }
    }

    async fn run_offers(
        &self,
        send: mesh::Sender<TaggedStream<u64, mesh::Receiver<ChannelRequest>>>,
    ) {
        while let Ok(action) = self.proxy.next_action().await {
            tracing::debug!(action = ?action, "action");
            match action {
                ProxyAction::Offer {
                    id,
                    offer,
                    incoming_event,
                    outgoing_event: _,
                } => {
                    if let Some(recv) = self.handle_offer(id, offer, incoming_event).await {
                        send.send(TaggedStream::new(id, recv));
                    }
                }
                ProxyAction::Revoke { id } => {
                    self.handle_revoke(id).await;
                }
                ProxyAction::InterruptPolicy {} => {}
            }
        }
    }

    async fn handle_request(&self, proxy_id: u64, request: Option<ChannelRequest>) {
        match request {
            Some(request) => {
                match request {
                    ChannelRequest::Open(rpc) => {
                        rpc.handle(|open_request| async move {
                            let result = self.handle_open(proxy_id, &open_request).await;
                            result.is_ok()
                        })
                        .await
                    }
                    ChannelRequest::Close(rpc) => {
                        rpc.handle(|()| async {
                            self.handle_close(proxy_id).await;
                        })
                        .await
                    }
                    ChannelRequest::Gpadl(rpc) => {
                        rpc.handle(|gpadl| async move {
                            let result = self
                                .handle_gpadl_create(proxy_id, gpadl.id, gpadl.count, &gpadl.buf)
                                .await;
                            result.is_ok()
                        })
                        .await
                    }
                    ChannelRequest::TeardownGpadl(rpc) => {
                        rpc.handle(|id| async move {
                            self.handle_gpadl_teardown(proxy_id, id).await;
                        })
                        .await
                    }
                    // Modifying the target VP is handle by the server, there is nothing the proxy
                    // driver needs to do.
                    ChannelRequest::Modify(Rpc(_, response)) => response.send(0),
                }
            }
            None => {
                // Due to a bug in vmbusproxy, this causes bugchecks if
                // there are any GPADLs still registered. This seems to
                // happen during teardown.
                let _ = self.proxy.close(proxy_id).await;
                let gpadls = self.gpadls.lock().remove(&proxy_id);
                if let Some(gpadls) = gpadls {
                    if !gpadls.is_empty() {
                        tracing::warn!(proxy_id, "revoke while some gpadls are still registered");
                        for gpadl_id in gpadls {
                            self.proxy
                                .delete_gpadl(proxy_id, gpadl_id.0)
                                .await
                                .expect("delete gpadl failed");
                        }
                    }
                }

                self.proxy
                    .release(proxy_id)
                    .await
                    .expect("channel release failed");

                self.channels.lock().remove(&proxy_id);
            }
        }
    }

    async fn run_channel_requests(
        self: &Arc<Self>,
        spawner: impl Spawn,
        mut recv: mesh::Receiver<TaggedStream<u64, mesh::Receiver<ChannelRequest>>>,
    ) {
        let mut channel_requests = SelectAll::new();

        'outer: loop {
            let (proxy_id, request) = loop {
                futures::select! { // merge semantics
                    r = recv.select_next_some() => {
                        channel_requests.push(r);
                    }
                    r = channel_requests.select_next_some() => break r,
                    complete => break 'outer,
                }
            };

            let this = self.clone();
            spawner
                .spawn("vmbus-proxy-req", async move {
                    this.handle_request(proxy_id, request).await
                })
                .detach();
        }
    }
}

async fn proxy_thread(spawner: impl Spawn, proxy: VmbusProxy, server: Arc<VmbusServerControl>) {
    let (send, recv) = mesh::channel();
    let proxy = Arc::new(proxy);
    let task = Arc::new(ProxyTask::new(server, proxy));
    let offers = task.run_offers(send);
    let requests = task.run_channel_requests(spawner, recv);
    futures::future::join(offers, requests).await;
    // BUGBUG: cancel all IO if something goes wrong?
}
