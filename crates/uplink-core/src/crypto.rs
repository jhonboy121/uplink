//! TLS crypto for iroh: aws-lc-rs with hybrid post-quantum key exchange preferred.
//!
//! X25519 stays in the provider only for relay/HTTPS servers without ML-KEM. Peer connections
//! must negotiate the hybrid group ([`require_post_quantum`]); anything else is refused.

use std::sync::Arc;

use iroh::endpoint::Connection;
use noq_proto::crypto::rustls::HandshakeData;
use rustls::NamedGroup;
use rustls::crypto::{CryptoProvider, aws_lc_rs};

use crate::Error;

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(CryptoProvider {
        kx_groups: vec![aws_lc_rs::kx_group::X25519MLKEM768, aws_lc_rs::kx_group::X25519],
        ..aws_lc_rs::default_provider()
    })
}

/// Key exchange group negotiated for `connection`, once its handshake has completed.
pub fn key_exchange_group(connection: &Connection) -> Option<NamedGroup> {
    connection.handshake_data()?.downcast::<HandshakeData>().ok()?.negotiated_key_exchange_group
}

pub const fn is_post_quantum(group: NamedGroup) -> bool {
    matches!(group, NamedGroup::X25519MLKEM768)
}

/// The negotiated group if it is post-quantum, else [`Error::NotPostQuantum`].
pub fn require_post_quantum(connection: &Connection) -> Result<NamedGroup, Error> {
    match key_exchange_group(connection) {
        Some(group) if is_post_quantum(group) => Ok(group),
        group => Err(Error::NotPostQuantum(group)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_prefers_hybrid_post_quantum() {
        let groups: Vec<NamedGroup> = provider().kx_groups.iter().map(|g| g.name()).collect();
        assert_eq!(groups, [NamedGroup::X25519MLKEM768, NamedGroup::X25519]);
    }

    #[test]
    fn only_the_hybrid_group_counts_as_post_quantum() {
        assert!(is_post_quantum(NamedGroup::X25519MLKEM768));
        assert!(!is_post_quantum(NamedGroup::X25519));
        assert!(!is_post_quantum(NamedGroup::secp256r1));
    }
}
