//! Themis client
use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_POINT, ristretto::RistrettoPoint, scalar::Scalar,
};
use elgamal_ristretto::{/*ciphertext::Ciphertext,*/ private::SecretKey, public::PublicKey};
use solana_client::{client_error::Result as ClientResult, rpc_client::RpcClient};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    message::Message,
    native_token::sol_to_lamports,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use spl_themis_ristretto::{
    instruction,
    state::generate_keys, // recover_scalar, User},
};
use std::time::Instant;

fn assert_transaction_size(tx: &Transaction) {
    let tx_size = bincode::serialize(&tx).unwrap().len();
    assert!(
        tx_size <= 1200,
        "transaction over 1200 bytes: {} bytes",
        tx_size
    );
}

pub fn send_and_confirm_transactions_with_spinner(
    rpc_client: &RpcClient,
    transactions: Vec<Transaction>,
    commitment: CommitmentConfig,
    last_valid_slot: solana_sdk::clock::Slot,
) -> ClientResult<()> {
    use bincode::serialize;
    use solana_cli::send_tpu::{get_leader_tpu, send_transaction_tpu};
    use solana_cli_output::display::new_spinner_progress_bar;
    use solana_client::{
        client_error::ClientErrorKind, rpc_request::MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS,
        rpc_response::RpcLeaderSchedule,
    };
    use std::{cmp::min, collections::HashMap, net::UdpSocket, thread::sleep, time::Duration};

    let progress_bar = new_spinner_progress_bar();
    let mut leader_schedule: Option<RpcLeaderSchedule> = None;
    let send_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    let cluster_nodes = rpc_client.get_cluster_nodes().ok();

    progress_bar.set_message("Finding leader node...");
    let epoch_info = rpc_client.get_epoch_info_with_commitment(commitment)?;
    if epoch_info.epoch > 0 || leader_schedule.is_none() {
        leader_schedule = rpc_client
            .get_leader_schedule_with_commitment(Some(epoch_info.absolute_slot), commitment)?;
    }
    let tpu_address = get_leader_tpu(
        min(epoch_info.slot_index + 1, epoch_info.slots_in_epoch),
        leader_schedule.as_ref(),
        cluster_nodes.as_ref(),
    )
    .unwrap();

    // Send all transactions
    let mut pending_transactions = HashMap::new();
    let num_transactions = transactions.len();
    for transaction in transactions {
        let wire_transaction = serialize(&transaction).expect("serialization should succeed");
        send_transaction_tpu(&send_socket, &tpu_address, &wire_transaction);
        pending_transactions.insert(transaction.signatures[0], wire_transaction);

        progress_bar.set_message(&format!(
            "[{}/{}] Total Transactions sent",
            pending_transactions.len(),
            num_transactions
        ));
    }

    // Collect statuses for all the transactions, drop those that are confirmed
    loop {
        // Retry once a second
        sleep(Duration::from_millis(1000));

        progress_bar.set_message(&format!(
            "[{}/{}] Transactions confirmed",
            num_transactions - pending_transactions.len(),
            num_transactions
        ));

        let mut statuses = vec![];
        let pending_signatures = pending_transactions.keys().cloned().collect::<Vec<_>>();
        for pending_signatures_chunk in
            pending_signatures.chunks(MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS - 1)
        {
            statuses.extend(
                rpc_client
                    .get_signature_statuses(pending_signatures_chunk)?
                    .value
                    .into_iter(),
            );
        }
        assert_eq!(statuses.len(), pending_signatures.len());

        for (signature, status) in pending_signatures.into_iter().zip(statuses.into_iter()) {
            if let Some(status) = status {
                if status.confirmations.is_none() || status.confirmations.unwrap() > 1 {
                    let _ = pending_transactions.remove(&signature);
                }
            }
            progress_bar.set_message(&format!(
                "[{}/{}] Transactions confirmed",
                num_transactions - pending_transactions.len(),
                num_transactions
            ));
        }

        if pending_transactions.is_empty() {
            return Ok(());
        }

        let slot = rpc_client.get_slot_with_commitment(commitment)?;
        if slot > last_valid_slot {
            break;
        }

        // TODO: Don't resend so much. Implement exponential backoff.
        for wire_transaction in pending_transactions.values() {
            send_transaction_tpu(&send_socket, &tpu_address, &wire_transaction);
        }
    }

    return Err(ClientErrorKind::Custom("Transactions failed".to_string()).into());
}

/// For each user, create interactions, calculate the aggregate, submit a proof, and verify it.
fn run_user_workflow(
    client: &RpcClient,
    program_id: &Pubkey,
    sender_keypairs: &[Keypair],
    (_sk, pk): (SecretKey, PublicKey),
    interactions: Vec<(RistrettoPoint, RistrettoPoint)>,
    policies_pubkey: Pubkey,
    _expected_scalar_aggregate: Scalar,
) -> ClientResult<usize> {
    let mut num_transactions = 0;
    let keys: Vec<_> = sender_keypairs
        .iter()
        .map(|sender_keypair| (sender_keypair, Keypair::new()))
        .collect();

    // Create each user's accounts
    let (recent_blockhash, _fee_calculator, last_valid_slot) = client
        .get_recent_blockhash_with_commitment(CommitmentConfig::default())?
        .value;
    let txs: Vec<_> = keys
        .iter()
        .map(|(sender_keypair, user_keypair)| {
            let sender_pubkey = sender_keypair.pubkey();
            let user_pubkey = user_keypair.pubkey();
            let ixs = instruction::create_user_account(
                program_id,
                &sender_pubkey,
                &user_pubkey,
                sol_to_lamports(0.001),
                pk,
            );
            let msg = Message::new(&ixs, Some(&sender_pubkey));
            Transaction::new(&[sender_keypair, user_keypair], msg, recent_blockhash)
        })
        .collect();
    num_transactions += txs.len();

    send_and_confirm_transactions_with_spinner(
        client,
        txs,
        CommitmentConfig::recent(),
        last_valid_slot,
    )
    .unwrap();

    // Send one interaction at a time to stay under the BPF instruction limit
    let (recent_blockhash, _fee_calculator, last_valid_slot) = client
        .get_recent_blockhash_with_commitment(CommitmentConfig::default())?
        .value;
    let txs: Vec<_> = keys
        .iter()
        .flat_map(|(sender_keypair, user_keypair)| {
            let sender_pubkey = sender_keypair.pubkey();
            let user_pubkey = user_keypair.pubkey();
            interactions
                .iter()
                .enumerate()
                .map(|(i, interaction)| {
                    let interactions = vec![(i as u8, *interaction)];
                    let ix = instruction::submit_interactions(
                        program_id,
                        &user_pubkey,
                        &policies_pubkey,
                        interactions,
                    );
                    let msg = Message::new(&[ix], Some(&sender_pubkey));
                    Transaction::new(&[sender_keypair, user_keypair], msg, recent_blockhash)
                })
                .collect::<Vec<_>>()
        })
        .collect();
    num_transactions += txs.len();
    send_and_confirm_transactions_with_spinner(
        client,
        txs,
        CommitmentConfig::recent(),
        last_valid_slot,
    )
    .unwrap();

    let (recent_blockhash, _fee_calculator, last_valid_slot) = client
        .get_recent_blockhash_with_commitment(CommitmentConfig::default())?
        .value;
    let txs: Vec<_> = keys
        .iter()
        .map(|(sender_keypair, user_keypair)| {
            let sender_pubkey = sender_keypair.pubkey();
            let user_pubkey = user_keypair.pubkey();
            //let user_account = client
            //    .get_account_with_commitment(
            //        user_pubkey,
            //        CommitmentConfig::recent(),
            //    )
            //    .unwrap()
            //    .unwrap();
            //let user = User::deserialize(&user_account.data).unwrap();
            //let ciphertext = Ciphertext {
            //    points: user.fetch_encrypted_aggregate(),
            //    pk,
            //};

            //let decrypted_aggregate = sk.decrypt(&ciphertext);
            let decrypted_aggregate = RISTRETTO_BASEPOINT_POINT;
            //let scalar_aggregate = recover_scalar(decrypted_aggregate, 16);
            //assert_eq!(scalar_aggregate, expected_scalar_aggregate);

            //let ((announcement_g, announcement_ctx), response) =
            //    sk.prove_correct_decryption_no_Merlin(&ciphertext, &decrypted_aggregate).unwrap();
            let ((announcement_g, announcement_ctx), response) = (
                (RISTRETTO_BASEPOINT_POINT, RISTRETTO_BASEPOINT_POINT),
                0u64.into(),
            );

            let ix = instruction::submit_proof_decryption(
                program_id,
                &user_pubkey,
                decrypted_aggregate,
                announcement_g,
                announcement_ctx,
                response,
            );
            let msg = Message::new(&[ix], Some(&sender_pubkey));
            Transaction::new(&[sender_keypair, user_keypair], msg, recent_blockhash)
        })
        .collect();
    num_transactions += txs.len();
    send_and_confirm_transactions_with_spinner(
        client,
        txs,
        CommitmentConfig::recent(),
        last_valid_slot,
    )
    .unwrap();

    //let user_account = client.get_account_with_commitment(user_pubkey, CommitmentConfig::recent()).unwrap().unwrap();
    //let user = User::deserialize(&user_account.data).unwrap();
    //assert!(user.fetch_proof_verification());

    Ok(num_transactions)
}

pub fn test_e2e(
    client: &RpcClient,
    program_id: &Pubkey,
    sender_keypair: Keypair,
    policies: Vec<Scalar>,
    num_users: u64,
    expected_scalar_aggregate: Scalar,
) -> ClientResult<()> {
    let sender_pubkey = sender_keypair.pubkey();
    let policies_keypair = Keypair::new();
    let policies_pubkey = policies_keypair.pubkey();
    let policies_len = policies.len();

    // Create the policies account
    let mut ixs = instruction::create_policies_account(
        program_id,
        &sender_pubkey,
        &policies_pubkey,
        sol_to_lamports(0.01),
        policies.len() as u8,
    );
    let policies_slice: Vec<_> = policies
        .iter()
        .enumerate()
        .map(|(i, x)| (i as u8, *x))
        .collect();
    ixs.push(instruction::store_policies(
        program_id,
        &policies_pubkey,
        policies_slice,
    ));

    let msg = Message::new(&ixs, Some(&sender_pubkey));
    let (recent_blockhash, _fee_calculator) = client.get_recent_blockhash()?;
    let tx = Transaction::new(&[&sender_keypair, &policies_keypair], msg, recent_blockhash);
    assert_transaction_size(&tx);
    client
        .send_and_confirm_transaction_with_spinner_and_commitment(&tx, CommitmentConfig::recent())
        .unwrap();

    // Send feepayer_keypairs some SOL
    println!("Seeding feepayer accounts...");
    let feepayers: Vec<_> = (0..num_users).map(|_| Keypair::new()).collect();
    let signer_keys = [&sender_keypair];
    let (recent_blockhash, _fee_calcualtor, last_valid_slot) = client
        .get_recent_blockhash_with_commitment(CommitmentConfig::default())
        .unwrap()
        .value;
    let txs: Vec<_> = feepayers
        .chunks(20)
        .map(|feepayers| {
            let payments: Vec<_> = feepayers
                .iter()
                .map(|keypair| (keypair.pubkey(), sol_to_lamports(0.0011)))
                .collect();
            let ixs = system_instruction::transfer_many(&sender_pubkey, &payments);
            let msg = Message::new(&ixs, Some(&sender_keypair.pubkey()));
            let tx = Transaction::new(&signer_keys, msg, recent_blockhash);
            assert_transaction_size(&tx);
            tx
        })
        .collect();
    send_and_confirm_transactions_with_spinner(
        client,
        txs,
        CommitmentConfig::recent(),
        last_valid_slot,
    )
    .unwrap();

    println!("Starting benchmark...");
    let now = Instant::now();

    let (sk, pk) = generate_keys();
    let interactions: Vec<_> = (0..policies_len)
        .map(|_| pk.encrypt(&RISTRETTO_BASEPOINT_POINT).points)
        .collect();

    let num_transactions = run_user_workflow(
        client,
        program_id,
        &feepayers,
        (sk.clone(), pk),
        interactions.clone(),
        policies_pubkey,
        expected_scalar_aggregate,
    )
    .unwrap();
    let elapsed = now.elapsed();
    println!("Benchmark complete.");

    println!(
        "{} transactions in {:?} ({} TPS)",
        num_transactions,
        elapsed,
        num_transactions as f64 / elapsed.as_secs_f64()
    );

    Ok(())
}
