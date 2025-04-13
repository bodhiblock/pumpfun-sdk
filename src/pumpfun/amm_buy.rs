use super::common::{
    amm_get_token_out, calculate_with_slippage_buy, get_amm_global_account_cache, get_amm_pool,
};
use crate::{
    common::{PriorityFee, SolanaRpcClient},
    constants::{self, accounts::WSOL, trade::DEFAULT_SLIPPAGE},
    instruction,
    swqos::FeeClient,
};
use anyhow::anyhow;
use solana_hash::Hash;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    native_token::sol_to_lamports,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
    transaction::{Transaction, VersionedTransaction},
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account_idempotent,
};
use spl_token::instruction::{close_account, sync_native};
use std::{str::FromStr, sync::Arc, time::Instant};
use tokio::task::JoinHandle;

const MAX_LOADED_ACCOUNTS_DATA_SIZE_LIMIT: u32 = 250000;

pub async fn buy(
    rpc: Arc<SolanaRpcClient>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    amount_sol: u64,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<(), anyhow::Error> {
    let transaction = build_buy_transaction(
        &rpc,
        &payer,
        &mint,
        amount_sol,
        slippage_basis_points,
        priority_fee,
    )
    .await?;
    rpc.send_and_confirm_transaction(&transaction).await?;
    Ok(())
}

pub async fn buy_with_tip(
    rpc: Arc<SolanaRpcClient>,
    fee_clients: Vec<Arc<FeeClient>>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    amount_sol: u64,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<(), anyhow::Error> {
    let start_time = Instant::now();

    let mint = Arc::new(mint.clone());
    let (pool, _) = get_amm_pool(&rpc, &mint).await?;
    let instructions = build_buy_instructions(
        &rpc,
        &payer,
        &mint,
        &pool,
        amount_sol,
        slippage_basis_points,
    )
    .await?;

    let mut transactions = vec![];
    let recent_blockhash = rpc.get_latest_blockhash().await?;
    for fee_client in fee_clients.clone() {
        let payer = payer.clone();
        let priority_fee = priority_fee.clone();
        let tip_account = fee_client
            .get_tip_account()
            .map_err(|e| anyhow!(e.to_string()))?;
        let tip_account = Arc::new(Pubkey::from_str(&tip_account).map_err(|e| anyhow!(e))?);

        let transaction = build_buy_transaction_with_tip(
            tip_account,
            payer,
            priority_fee,
            instructions.clone(),
            recent_blockhash,
        )?;
        transactions.push(transaction);
    }

    let mut handles: Vec<JoinHandle<Result<(), anyhow::Error>>> = vec![];
    for i in 0..fee_clients.len() {
        let fee_client = fee_clients[i].clone();
        let transactions = transactions.clone();
        let start_time = start_time.clone();
        let transaction = transactions[i].clone();
        let handle = tokio::spawn(async move {
            fee_client.send_transaction(&transaction).await?;
            println!(
                "index: {}, Total Jito buy operation time: {:?}ms",
                i,
                start_time.elapsed().as_millis()
            );
            Ok::<(), anyhow::Error>(())
        });

        handles.push(handle);
    }

    for handle in handles {
        match handle.await {
            Ok(Ok(_)) => (),
            Ok(Err(e)) => println!("Error in task: {}", e),
            Err(e) => println!("Task join error: {}", e),
        }
    }

    Ok(())
}

pub async fn buy_with_tip_ex(
    fee_clients: Vec<Arc<FeeClient>>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    pool: Pubkey,
    amount_sol: u64,
    amount_token: u64,
    slippage_basis_points: u64,
    priority_fee: PriorityFee,
    recent_blockhash: Hash,
) -> Result<(), anyhow::Error> {
    let start_time = Instant::now();

    let mint = Arc::new(mint.clone());
    let pool = Arc::new(pool.clone());
    let instructions = build_buy_instructions_ex(
        &payer,
        &mint,
        &pool,
        amount_sol,
        amount_token,
        slippage_basis_points,
    )?;

    let mut transactions = vec![];
    for fee_client in fee_clients.clone() {
        let payer = payer.clone();
        let priority_fee = priority_fee.clone();
        let tip_account = fee_client
            .get_tip_account()
            .map_err(|e| anyhow!(e.to_string()))?;
        let tip_account = Arc::new(Pubkey::from_str(&tip_account).map_err(|e| anyhow!(e))?);

        let transaction = build_buy_transaction_with_tip(
            tip_account,
            payer,
            priority_fee,
            instructions.clone(),
            recent_blockhash,
        )?;
        transactions.push(transaction);
    }

    let mut handles: Vec<JoinHandle<Result<(), anyhow::Error>>> = vec![];
    for i in 0..fee_clients.len() {
        let fee_client = fee_clients[i].clone();
        let transactions = transactions.clone();
        let start_time = start_time.clone();
        let transaction = transactions[i].clone();
        let handle = tokio::spawn(async move {
            fee_client.send_transaction(&transaction).await?;
            println!(
                "index: {}, Total Jito buy operation time: {:?}ms",
                i,
                start_time.elapsed().as_millis()
            );
            Ok::<(), anyhow::Error>(())
        });

        handles.push(handle);
    }

    for handle in handles {
        match handle.await {
            Ok(Ok(_)) => (),
            Ok(Err(e)) => println!("Error in task: {}", e),
            Err(e) => println!("Task join error: {}", e),
        }
    }

    Ok(())
}

pub async fn build_buy_transaction(
    rpc: &SolanaRpcClient,
    payer: &Keypair,
    mint: &Pubkey,
    amount_sol: u64,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<Transaction, anyhow::Error> {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
            MAX_LOADED_ACCOUNTS_DATA_SIZE_LIMIT,
        ),
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(priority_fee.unit_limit),
    ];
    let (pool, _) = get_amm_pool(&rpc, &mint).await?;
    let build_instructions =
        build_buy_instructions(rpc, payer, mint, &pool, amount_sol, slippage_basis_points).await?;
    instructions.extend(build_instructions);

    let recent_blockhash = rpc.get_latest_blockhash().await?;
    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    Ok(transaction)
}

pub fn build_buy_transaction_with_tip(
    tip_account: Arc<Pubkey>,
    payer: Arc<Keypair>,
    priority_fee: PriorityFee,
    build_instructions: Vec<Instruction>,
    blockhash: Hash,
) -> Result<VersionedTransaction, anyhow::Error> {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
            MAX_LOADED_ACCOUNTS_DATA_SIZE_LIMIT,
        ),
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(priority_fee.unit_limit),
        system_instruction::transfer(
            &payer.pubkey(),
            &tip_account,
            sol_to_lamports(priority_fee.buy_tip_fee),
        ),
    ];

    instructions.extend(build_instructions);

    let v0_message: v0::Message =
        v0::Message::try_compile(&payer.pubkey(), &instructions, &[], blockhash)?;
    let versioned_message: VersionedMessage = VersionedMessage::V0(v0_message);
    let transaction = VersionedTransaction::try_new(versioned_message, &[&payer])?;

    Ok(transaction)
}

pub async fn build_buy_instructions(
    rpc: &SolanaRpcClient,
    payer: &Keypair,
    mint: &Pubkey,
    pool: &Pubkey,
    amount_sol: u64,
    slippage_basis_points: Option<u64>,
) -> Result<Vec<Instruction>, anyhow::Error> {
    let pool_base_token_account = get_associated_token_address(pool, mint);
    let pool_quote_token_account = get_associated_token_address(pool, &WSOL);
    println!(
        "pool_base_token_account: {}, pool_quote_token_account: {}",
        pool_base_token_account.to_string(),
        pool_quote_token_account.to_string()
    );
    let (pool_base_account, pool_quote_account) = tokio::try_join!(
        rpc.get_token_account(&pool_base_token_account),
        rpc.get_token_account(&pool_quote_token_account)
    )?;

    if pool_base_account.is_none() {
        return Err(anyhow!("pool base account not found"));
    }

    if pool_quote_account.is_none() {
        return Err(anyhow!("pool quote account not found"));
    }

    let pool_base_reserve = u64::from_str(&pool_base_account.unwrap().token_amount.amount)?;
    let pool_quote_reserve = u64::from_str(&pool_quote_account.unwrap().token_amount.amount)?;
    let buy_amount = amm_get_token_out(pool_quote_reserve, pool_base_reserve, amount_sol);

    build_buy_instructions_ex(
        payer,
        mint,
        pool,
        amount_sol,
        buy_amount,
        slippage_basis_points.unwrap_or(DEFAULT_SLIPPAGE),
    )
}

pub fn build_buy_instructions_ex(
    payer: &Keypair,
    mint: &Pubkey,
    pool: &Pubkey,
    amount_sol: u64,
    amount_token: u64,
    slippage_basis_points: u64,
) -> Result<Vec<Instruction>, anyhow::Error> {
    if amount_sol == 0 {
        return Err(anyhow!("Amount cannot be zero"));
    }

    let global_account = get_amm_global_account_cache();
    let amount_sol = calculate_with_slippage_buy(amount_sol, slippage_basis_points);
    let mut instructions = vec![];

    instructions.push(create_associated_token_account_idempotent(
        &payer.pubkey(),
        &payer.pubkey(),
        &mint,
        &constants::accounts::TOKEN_PROGRAM,
    ));

    instructions.push(create_associated_token_account_idempotent(
        &payer.pubkey(),
        &payer.pubkey(),
        &WSOL,
        &constants::accounts::TOKEN_PROGRAM,
    ));

    let wsol_ata = get_associated_token_address(&payer.pubkey(), &WSOL);
    instructions.push(system_instruction::transfer(
        &payer.pubkey(),
        &wsol_ata,
        amount_sol,
    ));

    instructions.push(sync_native(&spl_token::ID, &wsol_ata).unwrap());

    instructions.push(instruction::amm_buy(
        payer,
        &mint,
        &pool,
        &global_account.protocol_fee_recipients[0],
        instruction::Buy {
            _amount: amount_token,
            _max_sol_cost: amount_sol,
        },
    ));

    instructions.push(
        close_account(
            &spl_token::ID,
            &wsol_ata,
            &payer.pubkey(),
            &payer.pubkey(),
            &[&payer.pubkey()],
        )
        .unwrap(),
    );

    Ok(instructions)
}
