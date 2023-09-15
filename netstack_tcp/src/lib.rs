use std::sync::Arc;

use netstack::network::{Ipv4, Ipv4Addr, Ipv4Type};
use netstack::transport::{Tcp, TcpFlags};

macro_rules! warn_on {
    ($condition:expr, $($fmt:tt)*) => {
        if $condition {
            log::warn!("check `{}` failed with {}", stringify!($condition), $($fmt)*);
            return;
        }
    };
}

#[derive(Default, Clone, PartialEq, Eq, Debug)]
pub enum State {
    /// Waiting for a connection request from any remote TCP peer and port.
    #[default]
    Listen,
    /// Waiting for a matching connection request after having sent a connection request
    SynSent,
    /// Waiting for a confirming connection request acknowledgment after having both received and
    /// sent a connection request.
    SynRecv,
    /// Open connection, data received can be delivered to the user. The normal state for the data
    /// transfer phase of the connection.
    Established,
    /// Waiting for a connection termination request from the remote TCP peer, or an acknowledgment
    /// of the connection termination request previously sent.
    FinWait1,
    /// Waiting for a connection termination request from the remote TCP peer.
    FinWait2,
    /// Waiting for a connection termination request from the local user.
    CloseWait,
    /// Waiting for a connection termination request acknowledgment from the remote TCP peer.
    Closing,
    /// Waiting for an acknowledgment of the connection termination request previously sent to the
    /// remote TCP peer (this termination request sent to the remote TCP peer already included an
    /// acknowledgment of the termination request sent from the remote TCP peer).
    LastAck,
    /// Waiting for enough time to pass to be sure the remote TCP peer received the acknowledgment
    /// of its connection termination request and to avoid new connections being impacted by
    /// delayed segments from previous connections.
    TimeWait,
    /// No connection state at all.
    Closed,
}

/// State of the Send Sequence Space (RFC 793 S3.2 F4)
///
/// ```
///            1         2          3          4
///       ----------|----------|----------|----------
///              SND.UNA    SND.NXT    SND.UNA
///                                   +SND.WND
///
/// 1 - old sequence numbers which have been acknowledged
/// 2 - sequence numbers of unacknowledged data
/// 3 - sequence numbers allowed for new data transmission
/// 4 - future sequence numbers which are not yet allowed
/// ```
#[derive(Default, Debug)]
pub struct SendSequenceSpace {
    /// Send unacknowledged.
    pub una: u32,
    /// Send next.
    pub nxt: u32,
    /// Send window.
    pub wnd: u16,
    /// Send urgent pointer.
    pub up: bool,
    /// Segment sequence number used for last window update.
    pub wl1: usize,
    /// Segment acknowledgment number used for last window update.
    pub wl2: usize,
    /// Initial send sequence number.
    pub iss: u32,
}

/// State of the Receive Sequence Space (RFC 793 S3.2 F5)
///
/// ```
///                1          2          3
///            ----------|----------|----------
///                   RCV.NXT    RCV.NXT
///                             +RCV.WND
///
/// 1 - old sequence numbers which have been acknowledged
/// 2 - sequence numbers allowed for new reception
/// 3 - future sequence numbers which are not yet allowed
/// ```
#[derive(Default, Debug)]
pub struct RecvSequenceSpace {
    /// Receive next.
    pub nxt: u32,
    /// Receive window.
    pub wnd: u16,
    /// Receive urgent pointer.
    pub up: bool,
    /// Initial receive sequence number.
    pub irs: u32,
}

#[derive(Debug)]
pub struct Address {
    pub src_port: u16,
    pub dest_port: u16,
    pub dest_ip: Ipv4Addr,
}

impl Address {
    #[inline]
    pub fn new(src_port: u16, dest_port: u16, dest_ip: Ipv4Addr) -> Self {
        Self {
            src_port,
            dest_port,
            dest_ip,
        }
    }
}

pub struct Socket<D: NetworkDevice> {
    state: State,
    recv: RecvSequenceSpace,
    send: SendSequenceSpace,

    addr: Address,
    pub device: Arc<D>,
}

impl<D: NetworkDevice> Socket<D> {
    pub fn new(device: Arc<D>, address: Address) -> Self {
        Self {
            device,
            recv: RecvSequenceSpace::default(),
            send: SendSequenceSpace::default(),
            state: State::default(),
            addr: address,
        }
    }

    pub fn connect(device: Arc<D>, address: Address) -> Self {
        let mut socket = Self {
            device,
            recv: RecvSequenceSpace::default(),
            send: SendSequenceSpace::default(),
            state: State::default(),
            addr: address,
        };

        socket.send_syn();
        socket
    }

    pub fn send_with_flags(&mut self, seq_number: u32, flags: TcpFlags) {
        let mut next_seq = seq_number;
        if flags.contains(TcpFlags::SYN) {
            next_seq = next_seq.wrapping_add(1);
        }

        if flags.contains(TcpFlags::FIN) {
            next_seq = next_seq.wrapping_add(1);
        }

        let ip = Ipv4::new(Ipv4Addr::NULL, self.addr.dest_ip, Ipv4Type::Tcp);
        let tcp = Tcp::new(self.addr.src_port, self.addr.dest_port)
            .set_flags(flags)
            .set_window(self.send.wnd)
            .set_sequence_number(seq_number)
            .set_ack_number(self.recv.nxt);

        if wrapping_lt(self.send.nxt, next_seq) {
            self.send.nxt = next_seq;
        }

        self.device.send(ip, tcp);
    }

    /// Send a SYN packet (connection request).
    fn send_syn(&mut self) {
        self.send.wnd = u16::MAX;
        self.send_with_flags(self.send.nxt, TcpFlags::SYN);
        self.state = State::SynSent;
    }

    pub fn close(&mut self) {
        match self.state {
            // connection already closed.
            State::Closed => return,
            // connection is closing.
            State::FinWait1
            | State::FinWait2
            | State::Closing
            | State::LastAck
            | State::TimeWait => return,

            State::Listen | State::SynSent => {
                // The connection has not been established yet, so we can just close it.
                self.state = State::Closed;
            }

            State::SynRecv | State::Established => {
                self.send_with_flags(self.send.nxt, TcpFlags::FIN | TcpFlags::ACK);
                self.state = State::FinWait1;
            }

            State::CloseWait => {
                self.send_with_flags(self.send.nxt, TcpFlags::FIN | TcpFlags::ACK);
                self.state = State::LastAck;
            }
        }
    }

    pub fn recv(&mut self, tcp: &Tcp, payload: &[u8]) {
        warn_on!(!self.validate_packet(tcp, payload), "invalid packet");

        let flags = tcp.flags();

        match self.state {
            State::Closed => return,

            State::Listen => {
                if flags.contains(TcpFlags::RST) {
                    // Incoming RST should be ignored.
                    return;
                }

                if flags.contains(TcpFlags::ACK) {
                    // Bad ACK; connection is still in the listen state.
                    self.send_with_flags(tcp.ack_number(), TcpFlags::RST);
                    return;
                }

                if !flags.contains(TcpFlags::SYN) {
                    // Expected a SYN packet.
                    return;
                }

                self.state = State::SynRecv;

                // Keep track of the sender info.
                self.recv.irs = tcp.sequence_number();
                self.recv.nxt = tcp.sequence_number() + 1;
                self.recv.wnd = tcp.window();

                // Initialize send sequence space.
                self.send.iss = 0;
                self.send.nxt = self.send.iss + 1;
                self.send.una = 0;
                self.send.wnd = u16::MAX;

                // Send SYN-ACK.
                self.send_with_flags(self.send.iss, TcpFlags::SYN | TcpFlags::ACK);
            }

            State::SynRecv => {
                if !flags.contains(TcpFlags::ACK) {
                    // Expected an ACK for the sent SYN.
                    return;
                }

                // ACKed the SYN (i.e, at least one acked byte and we have only sent the SYN).
                self.state = State::Established;
            }

            State::SynSent => {
                if flags.contains(TcpFlags::ACK | TcpFlags::SYN) {
                    self.recv.nxt = tcp.sequence_number().wrapping_add(1);
                    self.recv.irs = tcp.sequence_number();

                    self.send.una = tcp.ack_number();

                    if self.send.una > self.send.iss {
                        // TODO(andypython): Parse TCP options.
                        self.send.wnd = tcp.window();
                        self.send.wl1 = tcp.sequence_number() as usize;
                        self.send.wl2 = tcp.ack_number() as usize;
                        self.state = State::Established;

                        self.send_with_flags(self.send.nxt, TcpFlags::ACK);
                    }
                }
            }

            State::Established => {
                let seq_number = tcp.sequence_number();
                if seq_number != self.recv.nxt {
                    log::warn!("[ TCP ] Recieved out of order packet");
                    return;
                }

                // Advance RCV.NXT and adjust RCV.WND as apporopriate to the current buffer
                // availability.
                self.recv.nxt = seq_number.wrapping_add(payload.len() as u32);
                self.recv.wnd = u16::MAX;

                log::debug!("unread_data: {:?}", payload);
                self.send_with_flags(self.send.nxt, TcpFlags::ACK);
            }

            _ => {}
        }

        if flags.contains(TcpFlags::FIN) {
            match self.state {
                State::SynRecv | State::Established => {
                    self.state = State::CloseWait;
                }

                // The segment sequence number cannot be validated. Drop the segment and return.
                State::Closed | State::Listen | State::SynSent => return,

                _ => unimplemented!(),
            }
        }
    }

    fn validate_packet(&self, tcp: &Tcp, payload: &[u8]) -> bool {
        let flags = tcp.flags();

        if let State::Closed | State::Listen | State::SynSent = self.state {
            return true;
        }

        let ack_number = tcp.ack_number();
        let seq_number = tcp.sequence_number();

        let mut slen = payload.len() as u32;

        if flags.contains(TcpFlags::SYN) {
            slen += 1;
        }

        if flags.contains(TcpFlags::FIN) {
            slen += 1;
        }

        let wend = self.recv.nxt.wrapping_add(self.recv.wnd as u32);

        // Valid segment check.
        //
        // ```text
        // Length  Window
        // ------- -------  -------------------------------------------
        //
        //    0       0     SEG.SEQ = RCV.NXT
        //
        //    0      >0     RCV.NXT =< SEG.SEQ < RCV.NXT+RCV.WND
        //
        //   >0       0     not acceptable
        //
        //   >0      >0     RCV.NXT =< SEG.SEQ < RCV.NXT+RCV.WND
        //               or RCV.NXT =< SEG.SEQ+SEG.LEN-1 < RCV.NXT+RCV.WND
        // ```
        if slen == 0 {
            if self.recv.wnd == 0 && seq_number != self.recv.nxt {
                return false;
            } else if !is_between_wrapped(self.recv.nxt.wrapping_sub(1), seq_number, wend) {
                return false;
            }
        } else {
            if self.recv.wnd == 0 {
                return false;
            } else if !is_between_wrapped(self.recv.nxt.wrapping_sub(1), seq_number, wend)
                && !is_between_wrapped(
                    self.recv.nxt.wrapping_sub(1),
                    seq_number.wrapping_add(slen - 1),
                    wend,
                )
            {
                return false;
            }
        };

        // Acceptable ACK check.
        //      SND.UNA =< SEG.ACK =< SND.NXT
        if !is_between_wrapped(
            self.send.una.wrapping_sub(1),
            ack_number,
            self.send.nxt.wrapping_add(1),
        ) {
            return false;
        }

        true
    }

    #[inline]
    pub fn state(&self) -> State {
        self.state.clone()
    }
}

#[inline]
pub const fn wrapping_lt(lhs: u32, rhs: u32) -> bool {
    // From RFC 1323:
    //     TCP determines if a data segment is "old" or "new" by testing
    //     whether its sequence number is within 2**31 bytes of the left edge
    //     of the window, and if it is not, discarding the data as "old".  To
    //     insure that new data is never mistakenly considered old and vice-
    //     versa, the left edge of the sender's window has to be at most
    //     2**31 away from the right edge of the receiver's window.
    lhs.wrapping_sub(rhs) > 2 ^ 31
}

#[inline]
pub const fn is_between_wrapped(start: u32, x: u32, end: u32) -> bool {
    wrapping_lt(start, x) && wrapping_lt(x, end)
}

pub trait NetworkDevice: Send + Sync {
    fn send(&self, ipv4: Ipv4, tcp: Tcp);
}
