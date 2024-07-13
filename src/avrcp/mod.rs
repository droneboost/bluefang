use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use instructor::utils::u24;
use instructor::{BigEndian, Buffer, BufferMut, Instruct};
use parking_lot::Mutex;
use tokio::spawn;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{error, trace, warn};

use crate::avc::{CommandCode, Frame, Opcode, PassThroughFrame, Subunit, SubunitType};
use crate::avctp::{Avctp, Message, MessageType};
use crate::avrcp::error::NotImplemented;
use crate::avrcp::packets::{
    fragment_command, CommandAssembler, CommandStatus, Pdu, BLUETOOTH_SIG_COMPANY_ID, COMPANY_ID_CAPABILITY, EVENTS_SUPPORTED_CAPABILITY, PANEL
};
use crate::avrcp::session::{AvrcpCommand, CommandResponseSender, EventParser};
use crate::l2cap::channel::Channel;
use crate::l2cap::{ProtocolDelegate, ProtocolHandler, ProtocolHandlerProvider, AVCTP_PSM};
use crate::utils::{select2, Either2, LoggableResult, IgnoreableResult};
use crate::{ensure, hci};

mod error;
mod packets;
pub mod sdp;
mod session;

pub use error::{Error, ErrorCode};
pub use packets::{EventId, MediaAttributeId};
pub use session::{notifications, AvrcpSession, Event, Notification};
use crate::sdp::ids::service_classes::AV_REMOTE_CONTROL;

#[derive(Clone)]
pub struct Avrcp {
    existing_connections: Arc<Mutex<BTreeSet<u16>>>,
    session_handler: Arc<Mutex<dyn FnMut(AvrcpSession) + Send>>
}

impl ProtocolHandlerProvider for Avrcp {
    fn protocol_handlers(&self) -> Vec<Box<dyn ProtocolHandler>> {
        vec![ProtocolDelegate::boxed(AVCTP_PSM, self.clone(), Self::handle_control)]
    }
}

impl Avrcp {
    pub fn new<F: FnMut(AvrcpSession) + Send + 'static>(handler: F) -> Self {
        Self {
            existing_connections: Arc::new(Mutex::new(BTreeSet::new())),
            session_handler: Arc::new(Mutex::new(handler))
        }
    }

    fn handle_control(&self, mut channel: Channel) {
        let handle = channel.connection_handle();
        let success = self.existing_connections.lock().insert(handle);
        if success {
            if channel.accept_connection().log_err().is_err() {
                return;
            }
            let existing_connections = self.existing_connections.clone();
            let session_handler = self.session_handler.clone();
            spawn(async move {
                if let Err(err) = channel.configure().await {
                    warn!("Error configuring channel: {:?}", err);
                    return;
                }
                let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
                let (evt_tx, evt_rx) = tokio::sync::mpsc::channel(16);
                let mut state = State {
                    avctp: Avctp::new(channel, [AV_REMOTE_CONTROL]),
                    command_assembler: Default::default(),
                    response_assembler: Default::default(),
                    volume: MAX_VOLUME,
                    commands: cmd_rx,
                    events: evt_tx,
                    outstanding_transactions: Default::default(),
                    registered_notifications: Default::default()
                };
                session_handler.lock()(AvrcpSession {
                    commands: cmd_tx,
                    events: evt_rx
                });
                state.run().await.unwrap_or_else(|err| {
                    warn!("Error running avctp: {:?}", err);
                });
                trace!("AVCTP connection closed");
                existing_connections.lock().remove(&handle);
            });
        } else {
            channel.reject_connection().ignore();
        }
    }
}

#[derive(Default, Debug)]
enum TransactionState {
    #[default]
    Empty,
    PendingPassThrough(CommandResponseSender),
    PendingVendorDependent(CommandCode, CommandResponseSender),
    PendingNotificationRegistration(EventParser, CommandResponseSender),
    WaitingForChange(EventParser)
}

impl TransactionState {
    pub fn is_free(&self) -> bool {
        matches!(self, TransactionState::Empty)
    }

    pub fn take_sender(&mut self) -> CommandResponseSender {
        let prev = std::mem::take(self);
        match prev {
            TransactionState::PendingPassThrough(sender) => sender,
            TransactionState::PendingVendorDependent(_, sender) => sender,
            TransactionState::PendingNotificationRegistration(parser, sender) => {
                *self = TransactionState::WaitingForChange(parser);
                sender
            }
            _ => unreachable!()
        }
    }
}

struct State {
    avctp: Avctp,
    command_assembler: CommandAssembler,
    response_assembler: CommandAssembler,

    volume: u8,

    commands: Receiver<AvrcpCommand>,
    events: Sender<Event>,
    outstanding_transactions: [TransactionState; 16],
    registered_notifications: BTreeMap<EventId, u8>
}

impl State {
    async fn run(&mut self) -> Result<(), hci::Error> {
        loop {
            match select2(self.avctp.read(), self.commands.recv()).await {
                Either2::A(Some(mut packet)) => {
                    let transaction_label = packet.transaction_label;
                    if let Ok(frame) = packet.data.read_be::<Frame>() {
                        let payload = packet.data.clone();
                        if let Err(NotImplemented) = self.process_message(frame, packet).await {
                            if !frame.ctype.is_response() {
                                self.send_avc(
                                    transaction_label,
                                    Frame {
                                        ctype: CommandCode::NotImplemented,
                                        ..frame
                                    },
                                    payload
                                )
                                .await;
                            } else {
                                warn!("Failed to handle response: {:?}", frame);
                            }
                        }
                    }
                }
                Either2::B(Some(cmd)) => {
                    let Some(transaction) = self
                        .outstanding_transactions
                        .iter()
                        .position(|x| x.is_free())
                    else {
                        if let Some(sender) = cmd.into_response_sender() {
                            let _ = sender.send(Err(Error::NoTransactionIdAvailable));
                        }
                        continue;
                    };
                    match cmd {
                        AvrcpCommand::PassThrough(op, state, sender) => {
                            self.send_avc(
                                transaction as u8,
                                Frame {
                                    ctype: CommandCode::Control,
                                    subunit: PANEL,
                                    opcode: Opcode::PassThrough
                                },
                                PassThroughFrame { op, state, data_len: 0 }
                            )
                            .await
                            .then(|| self.outstanding_transactions[transaction] = TransactionState::PendingPassThrough(sender));
                        }
                        AvrcpCommand::VendorSpecific(cmd, pdu, params, sender) => {
                            // These should be registered using register notification
                            debug_assert!(cmd != CommandCode::Notify);
                            self.send_avrcp(transaction as u8, cmd, pdu, params)
                                .await
                                .then(|| self.outstanding_transactions[transaction] = TransactionState::PendingVendorDependent(cmd, sender));
                        }
                        AvrcpCommand::RegisterNotification(event, interval, parser, sender) => {
                            self.send_avrcp(transaction as u8, CommandCode::Notify, Pdu::RegisterNotification, (event, interval))
                                .await
                                .then(|| {
                                    self.outstanding_transactions[transaction] = TransactionState::PendingNotificationRegistration(parser, sender)
                                });
                        }
                        AvrcpCommand::UpdatedVolume(volume) => {
                            let new_volume = (volume.min(1.0).max(0.0) * MAX_VOLUME as f32).round() as u8;
                            if new_volume != self.volume {
                                self.volume = new_volume;
                                if let Some(transaction) = self
                                    .registered_notifications
                                    .remove(&EventId::VolumeChanged)
                                {
                                    self.send_avrcp(
                                        transaction,
                                        CommandCode::Changed,
                                        Pdu::RegisterNotification,
                                        (EventId::VolumeChanged, self.volume)
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
                _ => break
            }
        }
        Ok(())
    }

    async fn process_message(&mut self, frame: Frame, mut message: Message) -> Result<(), NotImplemented> {
        match frame.opcode {
            Opcode::VendorDependent => {
                ensure!(
                    frame.subunit == PANEL,
                    NotImplemented,
                    "Unsupported subunit: {:?}",
                    frame.subunit
                );
                let company_id: u24 = message.data.read_be::<u24>()?;
                ensure!(
                    company_id == BLUETOOTH_SIG_COMPANY_ID,
                    NotImplemented,
                    "Unsupported company id: {:#06x}",
                    company_id
                );
                if frame.ctype.is_response() {
                    match self.response_assembler.process_msg(message.data)? {
                        CommandStatus::Complete(pdu, mut parameters) => {
                            let transaction = &mut self.outstanding_transactions[message.transaction_label as usize];
                            match transaction {
                                TransactionState::PendingVendorDependent(CommandCode::Control, _) => {
                                    let reply = match frame.ctype {
                                        CommandCode::NotImplemented => Err(Error::NotImplemented),
                                        CommandCode::Accepted => Ok(parameters),
                                        CommandCode::Rejected => Err(Error::Rejected(parameters.read_be().unwrap_or(ErrorCode::ParameterContentError))),
                                        CommandCode::Interim => return Ok(()),
                                        _ => Err(Error::InvalidReturnData)
                                    };
                                    let _ = transaction.take_sender().send(reply);
                                }
                                TransactionState::PendingVendorDependent(CommandCode::Status, _) => {
                                    let reply = match frame.ctype {
                                        CommandCode::NotImplemented => Err(Error::NotImplemented),
                                        CommandCode::Implemented => Ok(parameters),
                                        CommandCode::Rejected => Err(Error::Rejected(parameters.read_be().unwrap_or(ErrorCode::ParameterContentError))),
                                        CommandCode::InTransition => Err(Error::Busy),
                                        _ => Err(Error::InvalidReturnData)
                                    };
                                    let _ = transaction.take_sender().send(reply);
                                }
                                TransactionState::PendingVendorDependent(code, _) => {
                                    error!("Received response for invalid command code: {:?}", code);
                                    *transaction = TransactionState::Empty;
                                }
                                TransactionState::PendingNotificationRegistration(_, _) => {
                                    let reply = match frame.ctype {
                                        CommandCode::NotImplemented => Err(Error::NotImplemented),
                                        CommandCode::Rejected => Err(Error::Rejected(parameters.read_be().unwrap_or(ErrorCode::ParameterContentError))),
                                        CommandCode::Interim => Ok(parameters),
                                        CommandCode::Changed => {
                                            warn!("Received changed response without interims response");
                                            Err(Error::InvalidReturnData)
                                        }
                                        _ => Err(Error::InvalidReturnData)
                                    };
                                    let _ = transaction.take_sender().send(reply);
                                }
                                TransactionState::WaitingForChange(parser) => {
                                    let parser = *parser;
                                    *transaction = TransactionState::Empty;
                                    if frame.ctype == CommandCode::Changed {
                                        let event = parameters
                                            .read_be::<EventId>()
                                            .and_then(|_| parser(&mut parameters))
                                            .map_err(|err| {
                                                error!("Error parsing event: {:?}", err);
                                            });
                                        if let Ok(event) = event {
                                            self.trigger_event(event);
                                        }
                                    }
                                }
                                _ => {
                                    warn!(
                                        "Received vendor dependent response with no/wrong outstanding transaction: {:?} {:?} {:?}",
                                        transaction, pdu, frame.ctype
                                    );
                                    return Ok(());
                                }
                            }
                        }
                        CommandStatus::Incomplete(pdu) => {
                            self.send_avrcp(message.transaction_label, CommandCode::Control, Pdu::RequestContinuingResponse, pdu)
                                .await;
                        }
                    }
                } else if let CommandStatus::Complete(pdu, parameters) = self.command_assembler.process_msg(message.data)? {
                    if let Err(err) = self
                        .process_command(message.transaction_label, frame.ctype, pdu, parameters)
                        .await
                    {
                        self.send_avrcp(message.transaction_label, CommandCode::Rejected, pdu, err)
                            .await;
                    }
                }

                Ok(())
            }
            Opcode::UnitInfo => {
                const UNIT_INFO: Subunit = Subunit {
                    ty: SubunitType::Unit,
                    id: 7
                };
                ensure!(
                    frame.ctype == CommandCode::Status,
                    NotImplemented,
                    "Unsupported command type: {:?}",
                    frame.ctype
                );
                ensure!(
                    frame.subunit == UNIT_INFO,
                    NotImplemented,
                    "Unsupported subunit: {:?}",
                    frame.subunit
                );
                self.send_avc(
                    message.transaction_label,
                    Frame {
                        ctype: CommandCode::Implemented,
                        subunit: UNIT_INFO,
                        opcode: Opcode::UnitInfo
                    },
                    (7u8, PANEL, BLUETOOTH_SIG_COMPANY_ID)
                )
                .await;
                Ok(())
            }
            Opcode::SubunitInfo => {
                const UNIT_INFO: Subunit = Subunit {
                    ty: SubunitType::Unit,
                    id: 7
                };
                ensure!(frame.ctype == CommandCode::Status, NotImplemented,"Unsupported command type: {:?}",frame.ctype);
                ensure!(frame.subunit == UNIT_INFO,NotImplemented,"Unsupported subunit: {:?}",frame.subunit);
                let page: u8 = message.data.read_be()?;
                self.send_avc(
                    message.transaction_label,
                    Frame {
                        ctype: CommandCode::Implemented,
                        subunit: UNIT_INFO,
                        opcode: Opcode::SubunitInfo
                    },
                    (page, PANEL, [0xffu8; 3])
                )
                .await;
                Ok(())
            }
            Opcode::PassThrough => {
                ensure!(frame.subunit == PANEL,NotImplemented,"Unsupported subunit: {:?}",frame.subunit);
                let transaction = &mut self.outstanding_transactions[message.transaction_label as usize];
                if !matches!(transaction, TransactionState::PendingPassThrough(_)) {
                    warn!("Received pass-through response with no/wrong outstanding transaction: {:?} {:?}", message, transaction);
                    return Ok(());
                }
                let _ = transaction.take_sender().send(match frame.ctype {
                    CommandCode::Accepted => Ok(message.data),
                    CommandCode::Rejected => Err(Error::Rejected(ErrorCode::NoError)),
                    CommandCode::NotImplemented => Err(Error::NotImplemented),
                    _ => Err(Error::InvalidReturnData)
                });
                Ok(())
            }
            code => {
                warn!("Unsupported opcode: {:?}", code);
                Err(NotImplemented)
            }
        }
    }

    async fn send_avrcp<I: Instruct<BigEndian>>(&mut self, transaction_label: u8, cmd: CommandCode, pdu: Pdu, parameters: I) -> bool {
        for packet in fragment_command(cmd, pdu, parameters) {
            let err = self
                .avctp
                .send_msg(Message {
                    transaction_label,
                    profile_id: AV_REMOTE_CONTROL,
                    message_type: match cmd.is_response() {
                        true => MessageType::Response,
                        false => MessageType::Command
                    },
                    data: packet
                })
                .await;
            if let Err(err) = err {
                warn!("Error sending command: {:?}", err);
                return false;
            }
        }
        true
    }

    async fn send_avc<I: Instruct<BigEndian>>(&mut self, transaction_label: u8, frame: Frame, parameters: I) -> bool {
        let mut buffer = BytesMut::new();
        buffer.write(frame);
        buffer.write(parameters);
        self.avctp
            .send_msg(Message {
                transaction_label,
                profile_id: AV_REMOTE_CONTROL,
                message_type: match frame.ctype.is_response() {
                    true => MessageType::Response,
                    false => MessageType::Command
                },
                data: buffer.freeze()
            })
            .await
            .map_err(|err| warn!("Error sending command: {:?}", err))
            .is_ok()
    }

    fn trigger_event(&self, event: Event) {
        if let Err(TrySendError::Full(event)) = self.events.try_send(event) {
            warn!("Event queue full, dropping event: {:?}", event);
        }
    }

    async fn process_command(&mut self, transaction: u8, _cmd: CommandCode, pdu: Pdu, mut parameters: Bytes) -> Result<(), ErrorCode> {
        match pdu {
            // ([AVRCP] Section 6.4.1)
            Pdu::GetCapabilities => {
                let capability: u8 = parameters.read_be()?;
                parameters.finish()?;
                match capability {
                    COMPANY_ID_CAPABILITY => {
                        self.send_avrcp(
                            transaction,
                            CommandCode::Implemented,
                            pdu,
                            (COMPANY_ID_CAPABILITY, 1, BLUETOOTH_SIG_COMPANY_ID)
                        )
                        .await;
                        Ok(())
                    }
                    EVENTS_SUPPORTED_CAPABILITY => {
                        //TODO Support a second event type to conform to spec
                        self.send_avrcp(
                            transaction,
                            CommandCode::Implemented,
                            pdu,
                            (EVENTS_SUPPORTED_CAPABILITY, 1u8, EventId::VolumeChanged)
                        )
                        .await;
                        Ok(())
                    }
                    _ => {
                        warn!("Unsupported capability: {}", capability);
                        Err(ErrorCode::InvalidParameter)
                    }
                }
            }
            // ([AVRCP] Section 6.7.2)
            Pdu::RegisterNotification => {
                // ensure!(cmd == CommandCode::Notify, ErrorCode::InvalidCommand);
                let event: EventId = parameters.read_be()?;
                let _: u32 = parameters.read_be()?;
                parameters.finish()?;
                ensure!(
                    !self.registered_notifications.contains_key(&event),
                    ErrorCode::InternalError,
                    "Event id already has a notification registered"
                );
                ensure!(
                    event == EventId::VolumeChanged,
                    ErrorCode::InvalidParameter,
                    "Attempted to register unsupported event: {:?}",
                    event
                );
                // ([AVRCP] Section 6.13.3)
                self.send_avrcp(transaction, CommandCode::Interim, pdu, (event, self.volume))
                    .await;
                self.registered_notifications.insert(event, transaction);
                Ok(())
            }
            // ([AVRCP] Section 6.8.1)
            Pdu::RequestContinuingResponse | Pdu::AbortContinuingResponse => {
                // Technically we have to delay parts of the response until these arrive but who cares
                Ok(())
            }
            // ([AVRCP] Section 6.13.2)
            Pdu::SetAbsoluteVolume => {
                self.volume = MAX_VOLUME.min(parameters.read_be()?);
                parameters.finish()?;
                self.send_avrcp(transaction, CommandCode::Accepted, pdu, self.volume)
                    .await;
                self.trigger_event(Event::VolumeChanged(self.volume as f32 / MAX_VOLUME as f32));
                Ok(())
            }
            _ => {
                warn!("Unsupported pdu: {:?}", pdu);
                Err(ErrorCode::InvalidCommand)
            }
        }
    }
}

const MAX_VOLUME: u8 = 0x7f;
