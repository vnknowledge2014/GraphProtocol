//! Race-free loopback port reservation handing over bound `127.0.0.1:0` listeners (`ARC-001`).

use std::net::{Ipv4Addr, SocketAddr, TcpListener};

use thiserror::Error;

/// Typed failure reserving or configuring a loopback listener.
#[derive(Debug, Error)]
pub enum PortAllocError {
    /// Failed to bind `127.0.0.1:0`.
    #[error("failed to bind loopback listener on 127.0.0.1:0: {0}")]
    Bind(#[source] std::io::Error),
    /// Failed to query the bound socket address.
    #[error("failed to read local_addr of bound loopback listener: {0}")]
    LocalAddr(#[source] std::io::Error),
    /// Failed to mark listener non-blocking prior to async reactor registration.
    #[error("failed to mark loopback listener non-blocking: {0}")]
    SetNonblocking(#[source] std::io::Error),
}

/// An already-bound `127.0.0.1:<port>` listener held open until consumed by the server under test.
///
/// Handing ownership of the bound [`TcpListener`] directly to `tokio::net::TcpListener::from_std`
/// (via [`ReservedListener::into_nonblocking_std`]) or a synchronous server (via
/// [`ReservedListener::into_std`]) eliminates the classic bind-then-close TOCTOU port collision race.
#[derive(Debug)]
pub struct ReservedListener {
    listener: TcpListener,
    local_addr: SocketAddr,
}

impl ReservedListener {
    /// Bound `127.0.0.1:<port>` socket address.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Assigned non-zero ephemeral TCP port on `127.0.0.1`.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.local_addr.port()
    }

    /// Formats `http://127.0.0.1:<port>` for HTTP client configuration.
    #[must_use]
    pub fn http_url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    /// Consumes the reservation and returns the bound [`std::net::TcpListener`] without closing it.
    #[must_use]
    pub fn into_std(self) -> TcpListener {
        self.listener
    }

    /// Marks the bound listener non-blocking (`set_nonblocking(true)`) and returns the still-open
    /// [`std::net::TcpListener`], ready for `tokio::net::TcpListener::from_std` with zero close gap.
    ///
    /// # Errors
    ///
    /// Returns [`PortAllocError::SetNonblocking`] if the OS rejects non-blocking configuration.
    pub fn into_nonblocking_std(self) -> Result<TcpListener, PortAllocError> {
        self.listener
            .set_nonblocking(true)
            .map_err(PortAllocError::SetNonblocking)?;
        Ok(self.listener)
    }
}

/// Allocates bound `127.0.0.1:0` TCP listeners without a bind-then-close race.
#[derive(Debug, Clone, Copy, Default)]
pub struct PortAllocator;

impl PortAllocator {
    /// Binds a new `127.0.0.1:0` listener and returns the live [`ReservedListener`].
    ///
    /// # Errors
    ///
    /// Returns [`PortAllocError`] if the OS refuses to bind or inspect the loopback socket.
    pub fn reserve() -> Result<ReservedListener, PortAllocError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(PortAllocError::Bind)?;
        let local_addr = listener.local_addr().map_err(PortAllocError::LocalAddr)?;
        Ok(ReservedListener {
            listener,
            local_addr,
        })
    }

    /// Simultaneously binds `count` distinct `127.0.0.1:0` listeners and returns them all open.
    ///
    /// # Errors
    ///
    /// Returns [`PortAllocError`] if any bind fails.
    pub fn reserve_many(count: usize) -> Result<Vec<ReservedListener>, PortAllocError> {
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(Self::reserve()?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io::ErrorKind;
    use std::net::IpAddr;

    use super::*;

    // spec: ARC-001
    #[test]
    fn reserves_bound_loopback_listener_without_bind_then_close_race() {
        let reserved = PortAllocator::reserve().expect("bind 127.0.0.1:0");
        assert_eq!(reserved.local_addr().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        let port = reserved.port();
        assert!(port > 0, "ephemeral port must be non-zero");
        assert_eq!(reserved.http_url(), format!("http://127.0.0.1:{port}"));

        // Prove there is NO bind-then-close race: re-binding the same port while `reserved` is held
        // MUST fail with `AddrInUse`.
        let collision = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .expect_err("port must remain bound while ReservedListener is alive");
        assert_eq!(collision.kind(), ErrorKind::AddrInUse);

        let nonblocking = reserved
            .into_nonblocking_std()
            .expect("mark bound listener nonblocking");
        assert_eq!(nonblocking.local_addr().expect("local_addr").port(), port);

        let collision_after_nonblocking = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .expect_err("port must remain bound after into_nonblocking_std");
        assert_eq!(collision_after_nonblocking.kind(), ErrorKind::AddrInUse);
    }

    // spec: ARC-001
    #[test]
    fn reserve_many_holds_distinct_bound_ports_simultaneously() {
        let listeners = PortAllocator::reserve_many(4).expect("reserve 4 loopback listeners");
        assert_eq!(listeners.len(), 4);

        let ports: BTreeSet<u16> = listeners.iter().map(ReservedListener::port).collect();
        assert_eq!(
            ports.len(),
            4,
            "all simultaneously held listeners must have distinct ports"
        );

        for listener in &listeners {
            let err = TcpListener::bind((Ipv4Addr::LOCALHOST, listener.port()))
                .expect_err("each reserved port must remain bound");
            assert_eq!(err.kind(), ErrorKind::AddrInUse);
        }

        let first = listeners.into_iter().next().expect("at least one listener");
        let port = first.port();
        let std_listener = first.into_std();
        assert_eq!(
            std_listener.local_addr().expect("std local_addr").port(),
            port
        );
    }
}
