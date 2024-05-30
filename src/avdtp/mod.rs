pub mod packets;
pub mod error;
pub mod endpoint;
pub mod utils;
pub mod capabilities;

use std::collections::BTreeMap;
use std::sync::Arc;
use bytes::{Bytes, BytesMut};
use instructor::{BigEndian, Buffer, BufferMut, Instruct};
use parking_lot::Mutex;
use tokio::{select, spawn};
use tokio::runtime::Handle;
use tokio::sync::oneshot::{Receiver, Sender};
use tracing::{debug, trace, warn};
use crate::avdtp::endpoint::Stream;
use crate::avdtp::error::ErrorCode;
use crate::avdtp::packets::{MessageType, ServiceCategory, SignalChannelExt, SignalIdentifier, SignalMessage, SignalMessageAssembler};
use crate::hci::Error;
use crate::l2cap::channel::Channel;
use crate::l2cap::Server;
use crate::utils::{MutexCell, select_all, stall_if_none};

pub use endpoint::{StreamHandler, LocalEndpoint};
use crate::avdtp::capabilities::Capability;
use crate::ensure;

#[derive(Default)]
pub struct AvdtpServerBuilder {
    endpoints: Vec<LocalEndpoint>,
}

impl AvdtpServerBuilder {

    pub fn with_endpoint(mut self, endpoint: LocalEndpoint) -> Self {
        self.endpoints.push(endpoint);
        self
    }

    pub fn build(self) -> AvdtpServer {
        AvdtpServer {
            pending_streams: Arc::new(Mutex::new(BTreeMap::new())),
            local_endpoints: self.endpoints.into(),
        }
    }
}

type ChannelSender = MutexCell<Option<Sender<Channel>>>;
pub struct AvdtpServer {
    pending_streams: Arc<Mutex<BTreeMap<u16, Arc<ChannelSender>>>>,
    local_endpoints: Arc<[LocalEndpoint]>,
}

impl Server for AvdtpServer {
    fn on_connection(&mut self, mut channel: Channel) {
        let handle = channel.connection_handle;
        let pending_stream = self.pending_streams.lock().get(&handle).cloned();
        match pending_stream {
            None => {
                trace!("New AVDTP session (signaling channel)");
                let pending_streams = self.pending_streams.clone();
                let pending_stream = Arc::new(ChannelSender::default());
                pending_streams.lock().insert(handle, pending_stream.clone());

                let local_endpoints = self.local_endpoints.clone();

                // Use an OS thread instead a tokio task to avoid blocking the runtime with audio processing
                let runtime = Handle::current();
                std::thread::spawn(move || runtime.block_on(async move {
                    if let Err(err) = channel.configure().await {
                        warn!("Error configuring channel: {:?}", err);
                        return;
                    }
                    let mut session = AvdtpSession {
                        channel_sender: pending_stream,
                        channel_receiver: None,
                        local_endpoints,
                        streams: Vec::new(),
                    };
                    session.handle_control_channel(channel).await.unwrap_or_else(|err| {
                        warn!("Error handling control channel: {:?}", err);
                    });
                    trace!("AVDTP signaling session ended for 0x{:04x}", handle);
                    pending_streams.lock().remove(&handle);
                }));
            }
            Some(pending) => {
                trace!("Existing AVDTP session (transport channel)");
                spawn(async move {
                    if let Err(err) = channel.configure().await {
                        warn!("Error configuring channel: {:?}", err);
                        return;
                    }
                    pending
                        .take()
                        .expect("Unexpected AVDTP transport connection")
                        .send(channel)
                        .unwrap_or_else(|_| panic!("Failed to send channel to session"));
                });
            }
        }
    }
}

struct AvdtpSession {
    channel_sender: Arc<ChannelSender>,
    channel_receiver: Option<Receiver<Channel>>,
    local_endpoints: Arc<[LocalEndpoint]>,
    streams: Vec<Stream>,
}

impl AvdtpSession {

    async fn handle_control_channel(&mut self, mut channel: Channel) -> Result<(), Error> {
        let mut assembler = SignalMessageAssembler::default();
        loop {
            select! {
                (i, _) = select_all(&mut self.streams) => {
                    debug!("Stream {} ended", i);
                    self.streams.swap_remove(i);
                },
                signal = channel.read() => match signal {
                    Some(packet) => match assembler.process_msg(packet) {
                        Ok(Some(header)) => {
                            let reply = self.handle_signal_message(header);
                            channel.send_signal(reply)?;
                        }
                        Ok(None) => continue,
                        Err(err) => {
                            warn!("Error processing signaling message: {:?}", err);
                            continue;
                        }
                    },
                    None => break,
                },
                res = stall_if_none(&mut self.channel_receiver) => {
                    let channel = res.expect("Channel receiver closed");
                    self.streams
                        .iter_mut()
                        .find(|stream| stream.is_opening())
                        .map(|stream| stream.set_channel(channel))
                        .unwrap_or_else(|| warn!("No stream waiting for channel"));
                    self.channel_receiver = None;
                }
            }
        }
        Ok(())
    }

    fn get_endpoint(&self, seid: u8) -> Result<&LocalEndpoint, ErrorCode> {
        self.local_endpoints.iter()
            .find(|ep| ep.seid == seid)
            .ok_or(ErrorCode::BadAcpSeid)
    }

    fn get_stream(&mut self, seid: u8) -> Result<&mut Stream, ErrorCode> {
        self.streams.iter_mut()
            .find(|stream| stream.local_endpoint == seid)
            .ok_or_else(|| self.local_endpoints.iter()
                .any(|ep| ep.seid == seid)
                .then_some(ErrorCode::BadState)
                .unwrap_or(ErrorCode::BadAcpSeid))
    }

    fn handle_signal_message(&mut self, msg: SignalMessage) -> SignalMessage {
        assert_eq!(msg.message_type, MessageType::Command);
        let resp = SignalMessageResponse::for_msg(&msg);
        let mut data = msg.data;
        match msg.signal_identifier {
            // ([AVDTP] Section 8.6).
            SignalIdentifier::Discover => resp.try_accept((), |buf, _| {
                data.finish()?;
                trace!("Got DISCOVER request");
                for endpoint in self.local_endpoints.iter() {
                    buf.write(&endpoint.as_stream_endpoint());
                }
                Ok(())
            }),
            // ([AVDTP] Section 8.7).
            SignalIdentifier::GetCapabilities => resp.try_accept((), |buf, _| {
                let seid = data.read_be::<u8>()? >> 2;
                data.finish()?;
                trace!("Got GET_CAPABILITIES request for 0x{:02x}", seid);
                let ep = self.get_endpoint(seid)?;
                ep.capabilities
                    .iter()
                    .filter(|cap| cap.is_basic())
                    .for_each(|cap| buf.write(cap));
                Ok(())
            }),
            // ([AVDTP] Section 8.8).
            SignalIdentifier::GetAllCapabilities => resp.try_accept((), |buf, _| {
                let seid = data.read_be::<u8>()? >> 2;
                data.finish()?;
                trace!("Got GET_ALL_CAPABILITIES request for 0x{:02x}", seid);
                let ep = self.get_endpoint(seid)?;
                buf.write(&ep.capabilities);
                Ok(())
            }),
            // ([AVDTP] Section 8.9).
            SignalIdentifier::SetConfiguration => resp.try_accept(ServiceCategory::Unknown, |_, _| {
                let acp_seid = data.read_be::<u8>()? >> 2;
                let int_seid = data.read_be::<u8>()? >> 2;
                let capabilities: Vec<Capability> = data.read_be()?;
                data.finish()?;
                trace!("Got SET_CONFIGURATION request for 0x{:02x} -> 0x{:02x}", acp_seid, int_seid);
                let ep = self.get_endpoint(acp_seid)?;
                ensure!(self.streams.iter().all(|stream| stream.local_endpoint != acp_seid), ErrorCode::BadState);
                self.streams.push(Stream::new(ep, int_seid, capabilities)?);
                Ok(())
            }),
            // ([AVDTP] Section 8.10).
            SignalIdentifier::GetConfiguration => resp.try_accept((), |buf, _| {
                let seid = data.read_be::<u8>()? >> 2;
                data.finish()?;
                trace!("Got GET_CONFIGURATION request for 0x{:02x}", seid);
                let stream = self.get_stream(seid)?;
                buf.write(stream.get_capabilities()?);
                Ok(())
            }),
            // ([AVDTP] Section 8.11).
            SignalIdentifier::Reconfigure => resp.try_accept(ServiceCategory::Unknown, |_, _| {
                let acp_seid = data.read_be::<u8>()? >> 2;
                let capabilities: Vec<Capability> = data.read_be()?;
                data.finish()?;
                trace!("Got RECONFIGURE request for 0x{:02x}", acp_seid);
                let ep = self.local_endpoints.iter()
                    .find(|ep| ep.seid == acp_seid)
                    .ok_or(ErrorCode::BadAcpSeid)?;
                let stream = self.streams.iter_mut()
                    .find(|stream| stream.local_endpoint == acp_seid)
                    .ok_or(ErrorCode::BadState)?;
                stream.reconfigure(capabilities, ep)?;
                Ok(())
            }),
            // ([AVDTP] Section 8.12).
            SignalIdentifier::Open => resp.try_accept((), |_, _| {
                let seid = data.read_be::<u8>()? >> 2;
                data.finish()?;
                trace!("Got OPEN request for 0x{:02x}", seid);
                let stream = self.get_stream(seid)?;
                stream.set_to_opening()?;
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.channel_sender.set(Some(tx));
                self.channel_receiver = Some(rx);
                Ok(())
            }),
            // ([AVDTP] Section 8.13).
            SignalIdentifier::Start => resp.try_accept(0x00u8, |_, ctx| {
                while {
                    let seid = data.read_be::<u8>()? >> 2;
                    data.finish()?;
                    *ctx = seid;
                    let sink = self.get_stream(seid)?;
                    trace!("Got START request for 0x{:02x}", seid);
                    sink.start()?;
                    !data.is_empty()
                } {}
                Ok(())
            }),
            // ([AVDTP] Section 8.14).
            SignalIdentifier::Close => resp.try_accept((), |_, _| {
                let seid = data.read_be::<u8>()? >> 2;
                data.finish()?;
                trace!("Got CLOSE request for 0x{:02x}", seid);
                let stream = self.get_stream(seid)?;
                stream.close()?;
                Ok(())
            }),
            // ([AVDTP] Section 8.15).
            SignalIdentifier::Suspend => resp.try_accept(0x00u8, |_, ctx| {
                while {
                    let seid = data.read_be::<u8>()? >> 2;
                    data.finish()?;
                    *ctx = seid;
                    trace!("Got SUSPEND request for 0x{:02x}", seid);
                    let sink = self.get_stream(seid)?;
                    sink.stop()?;
                    !data.is_empty()
                } {}
                Ok(())
            }),
            // ([AVDTP] Section 8.16).
            SignalIdentifier::Abort => resp.try_accept((), |_, _| {
                let seid = data.read_be::<u8>()? >> 2;
                data.finish()?;
                trace!("Got ABORT request for 0x{:02x}", seid);
                if let Some(id) = self.streams.iter_mut().position(|stream| stream.local_endpoint == seid) {
                    self.streams.swap_remove(id);
                }
                Ok(())
            }),
            // ([AVDTP] Section 8.17).
            SignalIdentifier::SecurityControl => resp.unsupported(),
            // ([AVDTP] Section 8.18).
            SignalIdentifier::Unknown => resp.general_reject(),
            // ([AVDTP] Section 8.19).
            SignalIdentifier::DelayReport => resp.unsupported()
        }
    }
}


struct SignalMessageResponse {
    transaction_label: u8,
    signal_identifier: SignalIdentifier,
}

impl SignalMessageResponse {

    pub fn for_msg(msg: &SignalMessage) -> Self {
        Self {
            transaction_label: msg.transaction_label,
            signal_identifier: msg.signal_identifier,
        }
    }

    pub fn general_reject(&self) -> SignalMessage {
        warn!("Unsupported signaling message: {:?}", self.signal_identifier);
        SignalMessage {
            transaction_label: self.transaction_label,
            message_type: MessageType::GeneralReject,
            signal_identifier: self.signal_identifier,
            data: Bytes::new(),
        }
    }

    pub fn unsupported(&self) -> SignalMessage {
        self.try_accept((), |_, _| Err(ErrorCode::NotSupportedCommand))
    }

    pub fn try_accept<F, C>(&self, err_ctx: C, f: F) -> SignalMessage
        where F: FnOnce(&mut BytesMut, &mut C) -> Result<(), ErrorCode>,
              C: Instruct<BigEndian>
    {
        let mut buf = BytesMut::new();
        let mut ctx = err_ctx;
        match f(&mut buf, &mut ctx) {
            Ok(()) => SignalMessage {
                transaction_label: self.transaction_label,
                message_type: MessageType::ResponseAccept,
                signal_identifier: self.signal_identifier,
                data: buf.freeze(),
            },
            Err(reason) => {
                warn!("Rejecting signal {:?} because of {:?}", self.signal_identifier, reason);
                buf.clear();
                buf.write_be(&ctx);
                buf.write_be(&reason);
                SignalMessage {
                    transaction_label: self.transaction_label,
                    message_type: MessageType::ResponseReject,
                    signal_identifier: self.signal_identifier,
                    data: buf.freeze(),
                }
            },
        }
    }

}
