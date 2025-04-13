use super::common::{
    calculate_with_slippage_sell, get_amm_global_account_cache, get_amm_pool, get_token_balance,
};
use crate::{
    common::{PriorityFee, SolanaRpcClient},
    constants::{self, accounts::WSOL, trade::DEFAULT_SLIPPAGE},
    instruction,
    pumpfun::common::amm_get_sol_out,
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
use spl_token::instruction::close_account;
use std::{str::FromStr, sync::Arc, time::Instant};
use tokio::task::JoinHandle;

pub async fn sell(
    rpc: Arc<SolanaRpcClient>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    amount_token: Option<u64>,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<(), anyhow::Error> {
    let transaction = build_sell_transaction(
        &rpc,
        &payer,
        &mint,
        amount_token,
        slippage_basis_points,
        priority_fee,
    )
    .await?;
    rpc.send_and_confirm_transaction(&transaction).await?;

    Ok(())
}

pub async fn sell_by_percent(
    rpc: Arc<SolanaRpcClient>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    percent: u64,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<(), anyhow::Error> {
    if percent == 0 || percent > 100 {
        return Err(anyhow!("Percentage must be between 1 and 100"));
    }

    let balance_u64 = get_token_balance(rpc.as_ref(), &payer.pubkey(), &mint).await?;
    let amount = balance_u64 * percent / 100;
    sell(
        rpc,
        payer,
        mint,
        Some(amount),
        slippage_basis_points,
        priority_fee,
    )
    .await
}

pub async fn sell_with_tip(
    fee_clients: Vec<Arc<FeeClient>>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    pool: Pubkey,
    amount_token: u64,
    amount_sol: u64,
    close_mint_ata: bool,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
    recent_blockhash: Hash,
) -> Result<(), anyhow::Error> {
    let start_time = Instant::now();

    let mut transactions = vec![];
    let instructions = build_sell_instructions_swap(
        &payer,
        &mint,
        &pool,
        amount_token,
        amount_sol,
        close_mint_ata,
        slippage_basis_points,
    )?;

    for fee_client in fee_clients.clone() {
        let payer = payer.clone();
        let priority_fee = priority_fee.clone();
        let tip_account = fee_client
            .get_tip_account()
            .map_err(|e| anyhow!(e.to_string()))?;
        let tip_account = Arc::new(Pubkey::from_str(&tip_account).map_err(|e| anyhow!(e))?);

        let transaction = build_sell_transaction_with_tip(
            tip_account,
            payer,
            priority_fee,
            instructions.clone(),
            recent_blockhash,
        )?;
        transactions.push(transaction);
    }

    let mut handles = vec![];
    for i in 0..fee_clients.len() {
        let fee_client = fee_clients[i].clone();
        let transaction = transactions[i].clone();
        let handle: JoinHandle<Result<(), anyhow::Error>> = tokio::spawn(async move {
            fee_client.send_transaction(&transaction).await?;
            println!(
                "index: {}, Total Jito sell operation time: {:?}ms",
                i,
                start_time.elapsed().as_millis()
            );
            Ok(())
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

    println!(
        "Total Jito sell operation time: {:?}ms",
        start_time.elapsed().as_millis()
    );
    Ok(())
}

pub async fn build_sell_transaction(
    rpc: &SolanaRpcClient,
    payer: &Keypair,
    mint: &Pubkey,
    amount_token: Option<u64>,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<Transaction, anyhow::Error> {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(priority_fee.unit_limit),
    ];
    let (pool, _) = get_amm_pool(&rpc, &mint).await?;
    let build_instructions = build_sell_instructions(
        rpc,
        payer,
        &mint,
        &pool,
        amount_token,
        slippage_basis_points,
    )
    .await?;

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

pub fn build_sell_transaction_with_tip(
    tip_account: Arc<Pubkey>,
    payer: Arc<Keypair>,
    priority_fee: PriorityFee,
    build_instructions: Vec<Instruction>,
    blockhash: Hash,
) -> Result<VersionedTransaction, anyhow::Error> {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(priority_fee.unit_limit),
        system_instruction::transfer(
            &payer.pubkey(),
            &tip_account,
            sol_to_lamports(priority_fee.sell_tip_fee),
        ),
    ];

    instructions.extend(build_instructions);

    let v0_message: v0::Message =
        v0::Message::try_compile(&payer.pubkey(), &instructions, &[], blockhash)?;
    let versioned_message: VersionedMessage = VersionedMessage::V0(v0_message);

    let transaction = VersionedTransaction::try_new(versioned_message, &[&payer])?;

    Ok(transaction)
}

pub async fn build_sell_instructions(
    rpc: &SolanaRpcClient,
    payer: &Keypair,
    mint: &Pubkey,
    pool: &Pubkey,
    amount_token: Option<u64>,
    slippage_basis_points: Option<u64>,
) -> Result<Vec<Instruction>, anyhow::Error> {
    let user_base_token_account = get_associated_token_address(&payer.pubkey(), mint);
    let pool_base_token_account = get_associated_token_address(pool, mint);
    let pool_quote_token_account = get_associated_token_address(pool, &WSOL);
    println!(
        "pool_base_token_account: {}, pool_quote_token_account: {}",
        pool_base_token_account.to_string(),
        pool_quote_token_account.to_string()
    );
    let (pool_base_account, pool_quote_account, user_base_account) = tokio::try_join!(
        rpc.get_token_account(&pool_base_token_account),
        rpc.get_token_account(&pool_quote_token_account),
        rpc.get_token_account(&user_base_token_account),
    )?;

    if pool_base_account.is_none() {
        return Err(anyhow!("pool base account not found"));
    }

    if pool_quote_account.is_none() {
        return Err(anyhow!("pool quote account not found"));
    }

    let pool_base_reserve = u64::from_str(&pool_base_account.unwrap().token_amount.amount)?;
    let pool_quote_reserve = u64::from_str(&pool_quote_account.unwrap().token_amount.amount)?;
    let user_base_balance = u64::from_str(&user_base_account.unwrap().token_amount.amount)?;
    let amount_token = amount_token.unwrap_or(user_base_balance);
    let amount_sol = amm_get_sol_out(pool_quote_reserve, pool_base_reserve, amount_token);

    build_sell_instructions_swap(
        payer,
        mint,
        pool,
        amount_token,
        amount_sol,
        amount_token == user_base_balance,
        slippage_basis_points,
    )
}

pub fn build_sell_instructions_swap(
    payer: &Keypair,
    mint: &Pubkey,
    pool: &Pubkey,
    amount_token: u64,
    amount_sol: u64,
    close_mint_ata: bool,
    slippage_basis_points: Option<u64>,
) -> Result<Vec<Instruction>, anyhow::Error> {
    if amount_token == 0 {
        return Err(anyhow!("Amount cannot be zero"));
    }

    let amount_sol = calculate_with_slippage_sell(
        amount_sol,
        slippage_basis_points.unwrap_or(DEFAULT_SLIPPAGE),
    );

    let mint_ata = get_associated_token_address(&payer.pubkey(), &mint);
    let wsol_ata = get_associated_token_address(&payer.pubkey(), &WSOL);
    let global_account = get_amm_global_account_cache();

    let mut instructions = vec![
        create_associated_token_account_idempotent(
            &payer.pubkey(),
            &payer.pubkey(),
            &WSOL,
            &constants::accounts::TOKEN_PROGRAM,
        ),
        instruction::amm_sell(
            payer,
            mint,
            pool,
            &global_account.protocol_fee_recipients[0],
            instruction::Sell {
                _amount: amount_token,
                _min_sol_output: amount_sol,
            },
        ),
    ];

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

    if close_mint_ata {
        instructions.push(
            close_account(
                &spl_token::ID,
                &mint_ata,
                &payer.pubkey(),
                &payer.pubkey(),
                &[&payer.pubkey()],
            )
            .unwrap(),
        );
    }

    Ok(instructions)
}
