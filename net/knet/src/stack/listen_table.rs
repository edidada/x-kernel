// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TCP listen table and backlog management.
use alloc::{collections::VecDeque, sync::Arc, vec, vec::Vec};

use hashbrown::HashMap;
use kerrno::{KError, KResult};
use kpoll::{PollContext, PollRegisterError, PollSet};
use ksync::Mutex;
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::tcp::{self, SocketBuffer, State},
    wire::{IpEndpoint, IpListenEndpoint},
};

use crate::{
    SOCKET_SET,
    consts::{LISTEN_QUEUE_SIZE, TCP_RX_BUF_LEN, TCP_TX_BUF_LEN},
};

/// Mutable listen backlog state. The accept wakeup source lives on
/// [`ListenEntry`] outside this mutex so register paths can clone it without
/// holding the queue lock.
struct ListenTableEntry {
    syn_queue: VecDeque<PendingConn>,
    accept_queue: VecDeque<PendingConn>,
    backlog: usize,
    touched: bool,
    closed: bool,
}

/// One listening endpoint: lock-free accept poll source plus guarded queues.
struct ListenEntry {
    /// Shared with waiters via [`PollSet::clone`]; not behind `state`.
    accept_poll: PollSet,
    /// Listener `sk_bound_dev_if`. Zero matches any arrival interface.
    bound_scope_id: u32,
    state: Mutex<ListenTableEntry>,
}

/// The result of accepting a TCP connection from the listen table.
#[derive(Clone, Copy)]
pub(crate) struct AcceptedTcp {
    pub(crate) handle: SocketHandle,
    pub(crate) local_endpoint: IpEndpoint,
    pub(crate) remote_endpoint: IpEndpoint,
    /// Interface the SYN arrived on. Used when the listener is unbound and
    /// the peer is IPv6 link-local (Linux `tcp_v6_init_req`).
    pub(crate) arrival_ifindex: u32,
}

struct PendingConn {
    accepted: AcceptedTcp,
}

impl ListenTableEntry {
    /// Create a new listen table entry for the given endpoint.
    fn new(backlog: usize) -> Self {
        let backlog = backlog.clamp(1, LISTEN_QUEUE_SIZE);
        Self {
            syn_queue: VecDeque::with_capacity(backlog),
            accept_queue: VecDeque::with_capacity(backlog),
            backlog,
            touched: false,
            closed: false,
        }
    }
}

impl ListenEntry {
    fn new(backlog: usize, bound_scope_id: u32) -> Self {
        Self {
            accept_poll: PollSet::new(),
            bound_scope_id,
            state: Mutex::new(ListenTableEntry::new(backlog)),
        }
    }
}

pub struct ListenTable {
    entries: Mutex<HashMap<IpListenEndpoint, Arc<ListenEntry>>>,
}

impl ListenTable {
    /// Create an empty listen table.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn can_listen(&self, endpoint: IpListenEndpoint) -> bool {
        let entries = self.entries.lock();
        !Self::listen_conflicts(&entries, endpoint)
    }

    /// Register a listener. `bound_scope_id` is the bind ifindex (`0` = any).
    pub fn listen(
        &self,
        listen_endpoint: IpListenEndpoint,
        backlog: usize,
        bound_scope_id: u32,
    ) -> KResult {
        let port = listen_endpoint.port;
        assert_ne!(port, 0);

        let mut entries = self.entries.lock();
        if Self::listen_conflicts(&entries, listen_endpoint) {
            warn!("socket already listening on {listen_endpoint:?}");
            return Err(KError::AddrInUse);
        }

        entries.insert(
            listen_endpoint,
            Arc::new(ListenEntry::new(backlog, bound_scope_id)),
        );
        Ok(())
    }

    pub fn unlisten(&self, endpoint: IpListenEndpoint) {
        let entry = {
            let mut entries = self.entries.lock();
            entries.remove(&endpoint)
        };

        let Some(entry) = entry else {
            return;
        };

        let handles = {
            let mut state = entry.state.lock();
            state.closed = true;
            state.drain_handles()
        };

        for handle in handles {
            SOCKET_SET.remove(handle);
        }
    }

    pub fn can_accept(&self, endpoint: IpListenEndpoint, sockets: &SocketSet<'_>) -> KResult<bool> {
        let Some(entry) = self.get_entry(&endpoint) else {
            warn!("accept before listen");
            return Err(KError::InvalidInput);
        };

        let state = entry.state.lock();
        Ok(state
            .accept_queue
            .iter()
            .any(|conn| is_acceptable_in_socket_set(conn.accepted.handle, sockets)))
    }

    pub fn register_accept_waker(
        &self,
        endpoint: IpListenEndpoint,
        sockets: &SocketSet<'_>,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        let Some(entry) = self.get_entry(&endpoint) else {
            warn!("register accept waker before listen");
            // Do not report success: sleeping without a registration hangs accept.
            return Err(PollRegisterError::InvalidState);
        };

        // Clone outside `state`: accept_poll is not behind the queue mutex.
        // Register first, then recheck readiness under the lock so a connection
        // that arrives between the sample and register still wakes us.
        context.register(&entry.accept_poll)?;
        let should_wake = {
            let state = entry.state.lock();
            state
                .accept_queue
                .iter()
                .any(|conn| is_acceptable_in_socket_set(conn.accepted.handle, sockets))
        };
        if should_wake {
            entry.accept_poll.wake();
        }
        Ok(())
    }

    pub fn accept(
        &self,
        endpoint: IpListenEndpoint,
        sockets: &mut SocketSet<'_>,
    ) -> KResult<AcceptedTcp> {
        let Some(entry) = self.get_entry(&endpoint) else {
            warn!("accept before listen");
            return Err(KError::InvalidInput);
        };

        let mut state = entry.state.lock();
        let mut has_aborted_conn = refresh_entry_for_accept(&mut state, sockets);

        loop {
            let conn = match state.accept_queue.pop_front() {
                Some(conn) => conn,
                None if has_aborted_conn => return Err(KError::ConnectionAborted),
                None => return Err(KError::WouldBlock),
            };
            if is_acceptable_in_socket_set(conn.accepted.handle, sockets) {
                return Ok(conn.accepted);
            }

            warn!("accept failed: connection aborted");
            sockets.remove(conn.accepted.handle);
            has_aborted_conn = true;
        }
    }

    pub fn note_tcp_packet(&self, dst: IpEndpoint) {
        let Some((_, entry)) = self.lookup_entry(dst) else {
            return;
        };

        entry.state.lock().touched = true;
    }

    pub fn wake_touched_acceptors(&self, sockets: &mut SocketSet<'_>) {
        let entries = {
            let entries = self.entries.lock();
            entries.values().cloned().collect::<Vec<_>>()
        };

        for entry in entries {
            let should_wake = {
                let mut state = entry.state.lock();
                if state.closed || !state.touched {
                    continue;
                }
                state.touched = false;
                refresh_entry_in_socket_set(&mut state, sockets)
            };
            if should_wake {
                entry.accept_poll.wake();
            }
        }
    }

    pub fn incoming_tcp_packet(
        &self,
        src: IpEndpoint,
        dst: IpEndpoint,
        arrival_ifindex: u32,
        sockets: &mut SocketSet<'_>,
    ) {
        let Some((endpoint, entry)) = self.lookup_entry(dst) else {
            return;
        };
        if !listen_scope_matches(entry.bound_scope_id, arrival_ifindex) {
            return;
        }

        let mut state = entry.state.lock();
        if state.closed {
            return;
        }

        prune_closed_in_socket_set(&mut state.syn_queue, sockets);
        prune_closed_in_socket_set(&mut state.accept_queue, sockets);
        if state
            .syn_queue
            .iter()
            .chain(state.accept_queue.iter())
            .any(|conn| conn.accepted.remote_endpoint == src && conn.accepted.local_endpoint == dst)
        {
            return;
        }
        // TODO(mivik): accept address check
        if state.syn_queue.len() + state.accept_queue.len() >= state.backlog {
            warn!("listen backlog overflow!");
            return;
        }

        let mut socket = smoltcp::socket::tcp::Socket::new(
            SocketBuffer::new(vec![0; TCP_RX_BUF_LEN]),
            SocketBuffer::new(vec![0; TCP_TX_BUF_LEN]),
        );
        if let Err(err) = socket.listen(IpListenEndpoint {
            addr: Some(dst.addr),
            port: dst.port,
        }) {
            warn!("Failed to listen on {}: {:?}", endpoint, err);
            return;
        }
        let dispatch_irq = sockets.add(socket);
        state.syn_queue.push_back(PendingConn {
            accepted: AcceptedTcp {
                handle: dispatch_irq,
                local_endpoint: dst,
                remote_endpoint: src,
                arrival_ifindex,
            },
        });
    }

    #[cfg(unittest)]
    fn queued_arrival_ifindex(&self, endpoint: IpListenEndpoint) -> Option<u32> {
        let entry = self.get_entry(&endpoint)?;
        let state = entry.state.lock();
        state
            .syn_queue
            .front()
            .or(state.accept_queue.front())
            .map(|conn| conn.accepted.arrival_ifindex)
    }

    fn get_entry(&self, endpoint: &IpListenEndpoint) -> Option<Arc<ListenEntry>> {
        let entries = self.entries.lock();
        entries.get(endpoint).cloned()
    }

    fn lookup_entry(&self, dst: IpEndpoint) -> Option<(IpListenEndpoint, Arc<ListenEntry>)> {
        let entries = self.entries.lock();
        Self::lookup_endpoint(&entries, dst).and_then(|endpoint| {
            entries
                .get(&endpoint)
                .cloned()
                .map(|entry| (endpoint, entry))
        })
    }

    fn lookup_endpoint(
        entries: &HashMap<IpListenEndpoint, Arc<ListenEntry>>,
        dst: IpEndpoint,
    ) -> Option<IpListenEndpoint> {
        // This only selects a candidate endpoint; callers must re-check the entry
        // after taking the listen table lock.
        let exact = IpListenEndpoint {
            addr: Some(dst.addr),
            port: dst.port,
        };
        if entries.contains_key(&exact) {
            return Some(exact);
        }

        let wildcard = IpListenEndpoint {
            addr: None,
            port: dst.port,
        };
        entries.contains_key(&wildcard).then_some(wildcard)
    }

    fn listen_conflicts(
        entries: &HashMap<IpListenEndpoint, Arc<ListenEntry>>,
        endpoint: IpListenEndpoint,
    ) -> bool {
        entries.keys().any(|old| {
            old.port == endpoint.port
                && (old.addr.is_none() || endpoint.addr.is_none() || old.addr == endpoint.addr)
        })
    }
}

impl ListenTableEntry {
    fn drain_handles(&mut self) -> Vec<SocketHandle> {
        let mut handles = Vec::with_capacity(self.syn_queue.len() + self.accept_queue.len());
        while let Some(conn) = self.syn_queue.pop_front() {
            handles.push(conn.accepted.handle);
        }
        while let Some(conn) = self.accept_queue.pop_front() {
            handles.push(conn.accepted.handle);
        }
        handles
    }
}

fn refresh_entry_in_socket_set(entry: &mut ListenTableEntry, sockets: &mut SocketSet<'_>) -> bool {
    prune_closed_in_socket_set(&mut entry.syn_queue, sockets);
    prune_closed_in_socket_set(&mut entry.accept_queue, sockets);
    promote_ready_in_socket_set(&mut entry.syn_queue, &mut entry.accept_queue, sockets)
}

fn refresh_entry_for_accept(entry: &mut ListenTableEntry, sockets: &mut SocketSet<'_>) -> bool {
    prune_closed_in_socket_set(&mut entry.syn_queue, sockets);
    let pruned_accept_queue = prune_closed_in_socket_set(&mut entry.accept_queue, sockets);
    promote_ready_in_socket_set(&mut entry.syn_queue, &mut entry.accept_queue, sockets);
    pruned_accept_queue
}

fn prune_closed_in_socket_set(
    syn_queue: &mut VecDeque<PendingConn>,
    sockets: &mut SocketSet<'_>,
) -> bool {
    let mut closed = Vec::with_capacity(syn_queue.len() / 4);
    syn_queue.retain(|conn| {
        let keep = !is_discardable_in_socket_set(conn.accepted.handle, sockets);
        if !keep {
            closed.push(conn.accepted.handle);
        }
        keep
    });
    let pruned = !closed.is_empty();
    for handle in closed {
        sockets.remove(handle);
    }
    pruned
}

fn is_discardable_in_socket_set(handle: SocketHandle, sockets: &SocketSet<'_>) -> bool {
    let socket = sockets.get::<tcp::Socket>(handle);
    match socket.state() {
        State::LastAck | State::TimeWait => true,
        State::Closed | State::FinWait1 | State::FinWait2 | State::Closing => {
            socket.recv_queue() == 0
        }
        State::Listen
        | State::SynSent
        | State::SynReceived
        | State::Established
        | State::CloseWait => false,
    }
}

fn is_acceptable_in_socket_set(handle: SocketHandle, sockets: &SocketSet<'_>) -> bool {
    is_acceptable_socket(sockets.get::<tcp::Socket>(handle))
}

fn is_acceptable_socket(socket: &tcp::Socket<'_>) -> bool {
    match socket.state() {
        State::Established | State::CloseWait => true,
        State::Closed | State::FinWait1 | State::FinWait2 | State::Closing => {
            socket.recv_queue() > 0
        }
        State::LastAck | State::TimeWait | State::Listen | State::SynSent | State::SynReceived => {
            false
        }
    }
}

/// Linux `inet_bound_dev_eq` without L3 master: an unbound listener matches
/// every interface; a bound listener matches only the SYN arrival ifindex.
fn listen_scope_matches(bound_scope_id: u32, arrival_ifindex: u32) -> bool {
    bound_scope_id == 0 || bound_scope_id == arrival_ifindex
}

fn promote_ready_in_socket_set(
    syn_queue: &mut VecDeque<PendingConn>,
    accept_queue: &mut VecDeque<PendingConn>,
    sockets: &SocketSet<'_>,
) -> bool {
    let mut moved = false;
    let len = syn_queue.len();
    for _ in 0..len {
        let Some(conn) = syn_queue.pop_front() else {
            break;
        };
        if is_acceptable_in_socket_set(conn.accepted.handle, sockets) {
            accept_queue.push_back(conn);
            moved = true;
        } else {
            syn_queue.push_back(conn);
        }
    }
    moved
}

#[cfg(unittest)]
mod tests {
    use smoltcp::wire::{IpAddress, Ipv6Address};
    use unittest::def_test;

    use super::*;

    fn wildcard(port: u16) -> IpListenEndpoint {
        IpListenEndpoint { addr: None, port }
    }

    fn v6_endpoint(host: u16, port: u16) -> IpEndpoint {
        IpEndpoint::new(
            IpAddress::Ipv6(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, host)),
            port,
        )
    }

    fn socket_count(sockets: &SocketSet<'_>) -> usize {
        sockets.iter().count()
    }

    #[def_test]
    fn listen_scope_matches_unbound_or_same_ifindex() {
        assert!(listen_scope_matches(0, 2));
        assert!(listen_scope_matches(2, 2));
        assert!(!listen_scope_matches(2, 3));
    }

    #[def_test]
    fn scoped_listen_ignores_syn_from_other_iface() {
        let table = ListenTable::new();
        let listen_ep = wildcard(18080);
        table.listen(listen_ep, 1, 2).unwrap();
        let mut sockets = SocketSet::new(vec![]);

        table.incoming_tcp_packet(
            v6_endpoint(1, 40000),
            v6_endpoint(2, 18080),
            3,
            &mut sockets,
        );
        assert_eq!(socket_count(&sockets), 0);
        assert!(table.queued_arrival_ifindex(listen_ep).is_none());

        table.incoming_tcp_packet(
            v6_endpoint(1, 40000),
            v6_endpoint(2, 18080),
            2,
            &mut sockets,
        );
        assert_eq!(socket_count(&sockets), 1);
        assert_eq!(table.queued_arrival_ifindex(listen_ep), Some(2));
    }

    #[def_test]
    fn unbound_listen_records_syn_arrival_ifindex() {
        let table = ListenTable::new();
        let listen_ep = wildcard(18081);
        table.listen(listen_ep, 1, 0).unwrap();
        let mut sockets = SocketSet::new(vec![]);

        table.incoming_tcp_packet(
            v6_endpoint(1, 40001),
            v6_endpoint(2, 18081),
            4,
            &mut sockets,
        );
        assert_eq!(table.queued_arrival_ifindex(listen_ep), Some(4));
    }
}
