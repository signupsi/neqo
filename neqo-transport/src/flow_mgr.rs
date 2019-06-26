// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Tracks possibly-redundant flow control signals from other code and converts
// into flow control frames needing to be sent to the peer.

use std::cmp::max;
use std::collections::HashMap;
use std::mem;
use std::time::Instant;

use neqo_common::{qinfo, qtrace, qwarn, Encoder};

use crate::frame::{Frame, FrameGenerator, FrameGeneratorToken, StreamType, TxMode};
use crate::stream_id::{StreamId, StreamIndex};
use crate::{AppError, Connection};

#[derive(Debug, Default)]
pub struct FlowMgr {
    // Discriminant as key ensures only 1 of every frame type will be queued.
    from_conn: HashMap<mem::Discriminant<Frame>, Frame>,

    // (id, discriminant) as key ensures only 1 of every frame type per stream
    // will be queued.
    from_streams: HashMap<(StreamId, mem::Discriminant<Frame>), Frame>,

    // (stream_type, discriminant) as key ensures only 1 of every frame type
    // per stream type will be queued.
    from_stream_types: HashMap<(StreamType, mem::Discriminant<Frame>), Frame>,

    used_data: u64,
    max_data: u64,

    need_close_frame: bool,
}

impl FlowMgr {
    pub fn conn_credit_avail(&self) -> u64 {
        self.max_data - self.used_data
    }

    pub fn conn_increase_credit_used(&mut self, amount: u64) {
        self.used_data += amount;
        assert!(self.used_data <= self.max_data)
    }

    pub fn conn_increase_max_credit(&mut self, new: u64) {
        self.max_data = max(self.max_data, new)
    }

    // -- frames scoped on connection --

    pub fn data_blocked(&mut self) {
        let frame = Frame::DataBlocked {
            data_limit: self.max_data,
        };
        self.from_conn.insert(mem::discriminant(&frame), frame);
    }

    pub fn path_response(&mut self, data: [u8; 8]) {
        let frame = Frame::PathResponse { data };
        self.from_conn.insert(mem::discriminant(&frame), frame);
    }

    // -- frames scoped on stream --

    /// Indicate to receiving peer the stream is reset
    pub fn stream_reset(
        &mut self,
        stream_id: StreamId,
        application_error_code: AppError,
        final_size: u64,
    ) {
        let frame = Frame::ResetStream {
            stream_id: stream_id.as_u64(),
            application_error_code,
            final_size,
        };
        self.from_streams
            .insert((stream_id, mem::discriminant(&frame)), frame);
    }

    /// Indicate to sending peer we are no longer interested in the stream
    pub fn stop_sending(&mut self, stream_id: StreamId, application_error_code: AppError) {
        let frame = Frame::StopSending {
            stream_id: stream_id.as_u64(),
            application_error_code,
        };
        self.from_streams
            .insert((stream_id, mem::discriminant(&frame)), frame);
    }

    /// Update sending peer with more credits
    pub fn max_stream_data(&mut self, stream_id: StreamId, maximum_stream_data: u64) {
        let frame = Frame::MaxStreamData {
            stream_id: stream_id.as_u64(),
            maximum_stream_data,
        };
        self.from_streams
            .insert((stream_id, mem::discriminant(&frame)), frame);
    }

    /// Indicate to receiving peer we need more credits
    pub fn stream_data_blocked(&mut self, stream_id: StreamId, stream_data_limit: u64) {
        let frame = Frame::StreamDataBlocked {
            stream_id: stream_id.as_u64(),
            stream_data_limit,
        };
        self.from_streams
            .insert((stream_id, mem::discriminant(&frame)), frame);
    }

    // -- frames scoped on stream type --

    pub fn max_streams(&mut self, stream_limit: StreamIndex, stream_type: StreamType) {
        let frame = Frame::MaxStreams {
            stream_type,
            maximum_streams: stream_limit,
        };
        self.from_stream_types
            .insert((stream_type, mem::discriminant(&frame)), frame);
    }

    pub fn streams_blocked(&mut self, stream_limit: StreamIndex, stream_type: StreamType) {
        let frame = Frame::StreamsBlocked {
            stream_type,
            stream_limit,
        };
        self.from_stream_types
            .insert((stream_type, mem::discriminant(&frame)), frame);
    }

    pub fn peek(&self) -> Option<&Frame> {
        if let Some(key) = self.from_conn.keys().next() {
            self.from_conn.get(key)
        } else if let Some(key) = self.from_streams.keys().next() {
            self.from_streams.get(key)
        } else if let Some(key) = self.from_stream_types.keys().next() {
            self.from_stream_types.get(key)
        } else {
            None
        }
    }

    pub(crate) fn need_close_frame(&self) -> bool {
        self.need_close_frame
    }

    pub(crate) fn set_need_close_frame(&mut self, new: bool) {
        self.need_close_frame = new
    }
}

impl Iterator for FlowMgr {
    type Item = Frame;
    /// Used by generator to get a flow control frame.
    fn next(&mut self) -> Option<Frame> {
        let first_key = self.from_conn.keys().next();
        if let Some(&first_key) = first_key {
            return self.from_conn.remove(&first_key);
        }

        let first_key = self.from_streams.keys().next();
        if let Some(&first_key) = first_key {
            return self.from_streams.remove(&first_key);
        }

        let first_key = self.from_stream_types.keys().next();
        if let Some(&first_key) = first_key {
            return self.from_stream_types.remove(&first_key);
        }

        None
    }
}

#[derive(Default)]
pub struct FlowControlGenerator {}

impl FrameGenerator for FlowControlGenerator {
    fn generate(
        &mut self,
        conn: &mut Connection,
        _now: Instant,
        _epoch: u16,
        _mode: TxMode,
        remaining: usize,
    ) -> Option<(Frame, Option<Box<FrameGeneratorToken>>)> {
        if let Some(frame) = conn.flow_mgr.borrow().peek() {
            // A suboptimal way to figure out if the frame fits within remaining
            // space.
            let mut d = Encoder::default();
            frame.marshal(&mut d);
            if d.len() > remaining {
                qtrace!("flowc frame doesn't fit in remaining");
                return None;
            }
        } else {
            return None;
        }
        // There is enough space we can add this frame to the packet.
        let frame = conn.flow_mgr.borrow_mut().next().expect("just peeked this");
        Some((
            frame.clone(),
            Some(Box::new(FlowControlGeneratorToken(frame))),
        ))
    }
}

struct FlowControlGeneratorToken(Frame);

impl FrameGeneratorToken for FlowControlGeneratorToken {
    fn acked(&mut self, conn: &mut Connection) {
        if let Frame::ResetStream {
            stream_id,
            application_error_code,
            final_size,
        } = self.0
        {
            qinfo!(
                [conn]
                "Reset received stream={} err={} final_size={}",
                stream_id,
                application_error_code,
                final_size
            );
            if let Some(ss) = conn.send_streams.get_mut(&stream_id.into()) {
                ss.reset_acked()
            }
        }
    }

    fn lost(&mut self, conn: &mut Connection) {
        match self.0 {
            // Always resend ResetStream if lost
            Frame::ResetStream {
                stream_id,
                application_error_code,
                final_size,
            } => {
                qinfo!(
                    [conn]
                    "Reset lost stream={} err={} final_size={}",
                    stream_id,
                    application_error_code,
                    final_size
                );
                if conn.send_streams.contains_key(&stream_id.into()) {
                    conn.flow_mgr.borrow_mut().stream_reset(
                        stream_id.into(),
                        application_error_code,
                        final_size,
                    );
                }
            }
            // Resend MaxStreams if lost (with updated value)
            Frame::MaxStreams { stream_type, .. } => {
                let local_max = match stream_type {
                    StreamType::BiDi => &mut conn.indexes.local_max_stream_bidi,
                    StreamType::UniDi => &mut conn.indexes.local_max_stream_uni,
                };

                conn.flow_mgr
                    .borrow_mut()
                    .max_streams(*local_max, stream_type)
            }
            // Only resend "*Blocked" frames if still blocked
            Frame::DataBlocked { .. } => {
                if conn.flow_mgr.borrow().conn_credit_avail() == 0 {
                    conn.flow_mgr.borrow_mut().data_blocked()
                }
            }
            Frame::StreamDataBlocked { stream_id, .. } => {
                if let Some(ss) = conn.send_streams.get(&stream_id.into()) {
                    if ss.credit_avail() == 0 {
                        conn.flow_mgr
                            .borrow_mut()
                            .stream_data_blocked(stream_id.into(), ss.max_stream_data())
                    }
                }
            }
            Frame::StreamsBlocked { stream_type, .. } => match stream_type {
                StreamType::UniDi => {
                    if conn.indexes.peer_next_stream_uni >= conn.indexes.peer_max_stream_uni {
                        conn.flow_mgr
                            .borrow_mut()
                            .streams_blocked(conn.indexes.peer_max_stream_uni, StreamType::UniDi);
                    }
                }
                StreamType::BiDi => {
                    if conn.indexes.peer_next_stream_bidi >= conn.indexes.peer_max_stream_bidi {
                        conn.flow_mgr
                            .borrow_mut()
                            .streams_blocked(conn.indexes.peer_max_stream_bidi, StreamType::BiDi);
                    }
                }
            },
            // Resend StopSending
            Frame::StopSending {
                stream_id,
                application_error_code,
            } => conn
                .flow_mgr
                .borrow_mut()
                .stop_sending(stream_id.into(), application_error_code),
            // Resend MaxStreamData if not SizeKnown
            // (maybe_send_flowc_update() checks this.)
            Frame::MaxStreamData { stream_id, .. } => {
                if let Some(rs) = conn.recv_streams.get_mut(&stream_id.into()) {
                    rs.maybe_send_flowc_update()
                }
            }
            Frame::PathResponse { .. } => qinfo!("Path Response lost, not re-sent"),
            _ => qwarn!("Unexpected Flow frame {:?} lost, not re-sent", self.0),
        }
    }
}