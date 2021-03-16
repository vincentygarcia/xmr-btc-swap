use crate::bitcoin::{
    poll_until_block_height_is_gte, BlockHeight, CancelTimelock, EncryptedSignature,
    PunishTimelock, TxCancel, TxLock, TxRefund,
};
use crate::protocol::alice;
use crate::protocol::alice::event_loop::EventLoopHandle;
use crate::protocol::alice::TransferProof;
use crate::{bitcoin, monero};
use anyhow::{Context, Result};
use futures::future::{select, Either};
use futures::pin_mut;
use libp2p::PeerId;

pub async fn lock_xmr(
    bob_peer_id: PeerId,
    state3: alice::State3,
    event_loop_handle: &mut EventLoopHandle,
    monero_wallet: &monero::Wallet,
) -> Result<()> {
    let S_a = monero::PublicKey::from_private_key(&monero::PrivateKey { scalar: state3.s_a });

    let public_spend_key = S_a + state3.S_b_monero;
    let public_view_key = state3.v.public();

    let transfer_proof = monero_wallet
        .transfer(public_spend_key, public_view_key, state3.xmr)
        .await?;

    // TODO(Franck): Wait for Monero to be confirmed once
    //  Waiting for XMR confirmations should not be done in here, but in a separate
    //  state! We have to record that Alice has already sent the transaction.
    //  Otherwise Alice might publish the lock tx twice!

    event_loop_handle
        .send_transfer_proof(bob_peer_id, TransferProof {
            tx_lock_proof: transfer_proof,
        })
        .await?;

    Ok(())
}

pub async fn wait_for_bitcoin_encrypted_signature(
    event_loop_handle: &mut EventLoopHandle,
) -> Result<EncryptedSignature> {
    let msg3 = event_loop_handle
        .recv_encrypted_signature()
        .await
        .context("Failed to receive Bitcoin encrypted signature from Bob")?;

    tracing::debug!("Message 3 received, returning it");

    Ok(msg3.tx_redeem_encsig)
}

pub async fn publish_cancel_transaction(
    tx_lock: TxLock,
    a: bitcoin::SecretKey,
    B: bitcoin::PublicKey,
    cancel_timelock: CancelTimelock,
    tx_cancel_sig_bob: bitcoin::Signature,
    bitcoin_wallet: &bitcoin::Wallet,
) -> Result<bitcoin::TxCancel> {
    // First wait for cancel timelock to expire
    let tx_lock_height = bitcoin_wallet
        .transaction_block_height(tx_lock.txid())
        .await?;
    poll_until_block_height_is_gte(bitcoin_wallet, tx_lock_height + cancel_timelock).await?;

    let tx_cancel = bitcoin::TxCancel::new(&tx_lock, cancel_timelock, a.public(), B);

    // If Bob hasn't yet broadcasted the tx cancel, we do it
    if bitcoin_wallet
        .get_raw_transaction(tx_cancel.txid())
        .await
        .is_err()
    {
        // TODO(Franck): Maybe the cancel transaction is already mined, in this case,
        // the broadcast will error out.

        let sig_a = a.sign(tx_cancel.digest());
        let sig_b = tx_cancel_sig_bob.clone();

        let tx_cancel = tx_cancel
            .clone()
            .add_signatures((a.public(), sig_a), (B, sig_b))
            .expect("sig_{a,b} to be valid signatures for tx_cancel");

        // TODO(Franck): Error handling is delicate, why can't we broadcast?
        bitcoin_wallet.broadcast(tx_cancel, "cancel").await?;

        // TODO(Franck): Wait until transaction is mined and returned mined
        // block height
    }

    Ok(tx_cancel)
}

pub async fn wait_for_bitcoin_refund(
    tx_cancel: &TxCancel,
    cancel_tx_height: BlockHeight,
    punish_timelock: PunishTimelock,
    refund_address: &bitcoin::Address,
    bitcoin_wallet: &bitcoin::Wallet,
) -> Result<(bitcoin::TxRefund, Option<bitcoin::Transaction>)> {
    let punish_timelock_expired =
        poll_until_block_height_is_gte(bitcoin_wallet, cancel_tx_height + punish_timelock);

    let tx_refund = bitcoin::TxRefund::new(tx_cancel, refund_address);

    // TODO(Franck): This only checks the mempool, need to cater for the case where
    // the transaction goes directly in a block
    let seen_refund_tx = bitcoin_wallet.watch_for_raw_transaction(tx_refund.txid());

    pin_mut!(punish_timelock_expired);
    pin_mut!(seen_refund_tx);

    match select(punish_timelock_expired, seen_refund_tx).await {
        Either::Left(_) => Ok((tx_refund, None)),
        Either::Right((published_refund_tx, _)) => Ok((tx_refund, Some(published_refund_tx?))),
    }
}

pub fn extract_monero_private_key(
    published_refund_tx: bitcoin::Transaction,
    tx_refund: &TxRefund,
    s_a: monero::Scalar,
    a: bitcoin::SecretKey,
    S_b_bitcoin: bitcoin::PublicKey,
) -> Result<monero::PrivateKey> {
    let s_a = monero::PrivateKey { scalar: s_a };

    let tx_refund_sig = tx_refund
        .extract_signature_by_key(published_refund_tx, a.public())
        .context("Failed to extract signature from Bitcoin refund tx")?;
    let tx_refund_encsig = a.encsign(S_b_bitcoin, tx_refund.digest());

    let s_b = bitcoin::recover(S_b_bitcoin, tx_refund_sig, tx_refund_encsig)
        .context("Failed to recover Monero secret key from Bitcoin signature")?;
    let s_b = monero::private_key_from_secp256k1_scalar(s_b.into());

    let spend_key = s_a + s_b;

    Ok(spend_key)
}
