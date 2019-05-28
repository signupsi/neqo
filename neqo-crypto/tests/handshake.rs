#![allow(dead_code)]

use neqo_common::qinfo;
use neqo_crypto::*;
use std::mem;
use std::time::Duration;

// Time in nanoseconds since epoch; we need enough to avoid underflow.
pub const NOW: u64 = 32_000_000;

// This needs to be > 2ms to avoid it being rounded to zero.
// NSS operates in milliseconds and halves any value it is provided.
pub const ANTI_REPLAY_WINDOW: Duration = Duration::from_millis(10);

pub fn forward_records(
    now: u64,
    agent: &mut SecretAgent,
    records_in: RecordList,
) -> Res<RecordList> {
    let mut expected_state = match agent.state() {
        HandshakeState::New => HandshakeState::New,
        _ => HandshakeState::InProgress,
    };
    let mut records_out = RecordList::default();
    for record in records_in.into_iter() {
        assert_eq!(records_out.len(), 0);
        assert_eq!(*agent.state(), expected_state);

        records_out = agent.handshake_raw(now, Some(record))?;
        expected_state = HandshakeState::InProgress;
    }
    Ok(records_out)
}

fn handshake(now: u64, client: &mut SecretAgent, server: &mut SecretAgent) {
    let mut a = client;
    let mut b = server;
    let mut records = a.handshake_raw(now, None).unwrap();
    let is_done = |agent: &mut SecretAgent| match *agent.state() {
        HandshakeState::Complete(_) | HandshakeState::Failed(_) => true,
        _ => false,
    };
    while !is_done(a) || !is_done(b) {
        records = match forward_records(now, &mut b, records) {
            Ok(r) => r,
            _ => {
                // TODO(mt) take the alert generated by the failed handshake
                // and allow it to be sent to the peer.
                return;
            }
        };

        if *b.state() == HandshakeState::AuthenticationPending {
            b.authenticated();
            records = b.handshake_raw(now, None).unwrap();
        }
        b = mem::replace(&mut a, b);
    }
}

pub fn connect_at(now: u64, client: &mut SecretAgent, server: &mut SecretAgent) {
    handshake(now, client, server);
    qinfo!("client: {:?}", client.state());
    qinfo!("server: {:?}", server.state());
    assert!(client.state().connected());
    assert!(server.state().connected());
}

pub fn connect(client: &mut SecretAgent, server: &mut SecretAgent) {
    connect_at(NOW, client, server);
}

pub fn connect_fail(client: &mut SecretAgent, server: &mut SecretAgent) {
    handshake(NOW, client, server);
    assert!(!client.state().connected());
    assert!(!server.state().connected());
}

#[derive(Clone, Copy, Debug)]
pub enum Resumption {
    WithoutZeroRtt,
    WithZeroRtt,
}

pub const ZERO_RTT_TOKEN_DATA: &[u8] = b"zero-rtt-token";

#[derive(Debug)]
pub struct PermissiveZeroRttChecker {
    resuming: bool
}
impl PermissiveZeroRttChecker {
    pub fn new() -> Box<dyn ZeroRttChecker> {
        Box::new(PermissiveZeroRttChecker {resuming: true})
    }
}
impl ZeroRttChecker for PermissiveZeroRttChecker {
    fn check(&self, first: bool, token: &[u8]) -> ZeroRttCheckResult {
        assert!(first);
        if self.resuming {
            assert_eq!(ZERO_RTT_TOKEN_DATA, token);
        } else {
            assert!(token.is_empty());
        }
        ZeroRttCheckResult::Accept
    }
}

pub fn resumption_setup(mode: Resumption) -> Vec<u8> {
    init_db("./db");
    // We need to pretend that initialization was in the past.
    // That way, the anti-replay filter is cleared when we try to connect at |NOW|.
    let start_time = NOW
        .checked_sub(ANTI_REPLAY_WINDOW.as_nanos() as u64)
        .unwrap();
    Server::init_anti_replay(start_time, ANTI_REPLAY_WINDOW, 1, 3)
        .expect("anti-replay setup successful");

    let mut client = Client::new("server.example").expect("should create client");
    let mut server = Server::new(&["key"]).expect("should create server");
    if let Resumption::WithZeroRtt = mode {
        client.enable_0rtt().expect("should enable 0-RTT");
        server
            .enable_0rtt(0xffffffff, Box::new(PermissiveZeroRttChecker{resuming: false}))
            .expect("should enable 0-RTT");
    }

    connect(&mut client, &mut server);

    assert!(!client.info().unwrap().resumed());
    assert!(!server.info().unwrap().resumed());
    assert!(!client.info().unwrap().early_data_accepted());
    assert!(!server.info().unwrap().early_data_accepted());

    let server_records = server
        .send_ticket(NOW, ZERO_RTT_TOKEN_DATA)
        .expect("ticket sent");
    assert_eq!(server_records.len(), 1);
    let client_records = client
        .handshake_raw(NOW, server_records.into_iter().next())
        .expect("records ingested");
    assert_eq!(client_records.len(), 0);

    client.resumption_token().expect("token is present").clone()
}
