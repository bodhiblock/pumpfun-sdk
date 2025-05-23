use anyhow::anyhow;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction, instruction::Instruction, message::{v0, VersionedMessage}, native_token::sol_to_lamports, pubkey::Pubkey, signature::Keypair, signer::Signer, system_instruction, transaction::{Transaction, VersionedTransaction}
};
use solana_hash::Hash;
use spl_associated_token_account::get_associated_token_address;
use spl_token::instruction::close_account;
use tokio::task::JoinHandle;
use std::{str::FromStr, time::Instant, sync::Arc};
use crate::{common::{PriorityFee, SolanaRpcClient}, constants::trade::DEFAULT_SLIPPAGE, instruction, swqos::FeeClient};
use super::common::{calculate_with_slippage_sell, get_bonding_curve_account, get_global_account, get_global_account_cache};

async fn get_token_balance(rpc: &SolanaRpcClient, payer: &Keypair, mint: &Pubkey) -> Result<(u64, Pubkey), anyhow::Error> {
    let ata = get_associated_token_address(&payer.pubkey(), mint);
    let balance = rpc.get_token_account_balance(&ata).await?;
    let balance_u64 = balance.amount.parse::<u64>()
        .map_err(|_| anyhow!("Failed to parse token balance"))?;
    
    if balance_u64 == 0 {
        return Err(anyhow!("Balance is 0"));
    }

    Ok((balance_u64, ata))
}

pub async fn sell(
    rpc: Arc<SolanaRpcClient>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    amount_token: Option<u64>,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<(), anyhow::Error> {
    let instructions = build_sell_instructions(rpc.clone(), payer.clone(), mint.clone(), amount_token, slippage_basis_points).await?;
    let transaction = build_sell_transaction(rpc.clone(), &payer, priority_fee, instructions).await?;
    rpc.send_and_confirm_transaction(&transaction).await?;

    Ok(())
}

pub fn sell_ex(
    rpc: Arc<SolanaRpcClient>,
    payer: &Keypair,
    mint: &Pubkey,
    amount_token: u64,
    amount_sol: u64,
    close_mint_ata: bool,
    slippage_basis_points: u64,
    priority_fee: PriorityFee,
    recent_blockhash: Hash,
) -> Result<String, anyhow::Error> {
    let instructions = build_sell_instructions_ex(&payer, &mint, amount_token, amount_sol, close_mint_ata, Some(slippage_basis_points))?;
    let transaction = build_sell_transaction_sync(payer, priority_fee, instructions, recent_blockhash);
    let tx_hash = transaction.signatures[0].to_string();
    tokio::spawn(async move {
        rpc.send_transaction(&transaction).await
    });
    Ok(tx_hash)
}

/// Sell tokens by percentage
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

    let (balance_u64, _) = get_token_balance(rpc.as_ref(), payer.as_ref(), &mint).await?;
    let amount = balance_u64 * percent / 100;
    sell(rpc, payer, mint, Some(amount), slippage_basis_points, priority_fee).await
}

pub async fn sell_by_percent_with_tip(
    rpc: Arc<SolanaRpcClient>,
    fee_clients: Vec<Arc<FeeClient>>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    percent: u64,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<(), anyhow::Error> {
    if percent == 0 || percent > 100 {
        return Err(anyhow!("Percentage must be between 1 and 100"));
    }

    let (balance_u64, _) = get_token_balance(rpc.as_ref(), payer.as_ref(), &mint).await?;
    let amount = balance_u64 * percent / 100;
    sell_with_tip(rpc, fee_clients, payer, mint, Some(amount), slippage_basis_points, priority_fee).await
}

/// Sell tokens using Jito
pub async fn sell_with_tip(
    rpc: Arc<SolanaRpcClient>,
    fee_clients: Vec<Arc<FeeClient>>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    amount_token: Option<u64>,
    slippage_basis_points: Option<u64>,
    priority_fee: PriorityFee,
) -> Result<(), anyhow::Error> {
    let start_time = Instant::now();

    let mut transactions = vec![];
    let instructions = build_sell_instructions(rpc.clone(), payer.clone(), mint.clone(), amount_token, slippage_basis_points).await?;

    let recent_blockhash = rpc.get_latest_blockhash().await?;
    for fee_client in fee_clients.clone() {
        let payer = payer.clone();
        let priority_fee = priority_fee.clone();
        let tip_account = fee_client.get_tip_account().map_err(|e| anyhow!(e.to_string()))?;
        let tip_account = Arc::new(Pubkey::from_str(&tip_account).map_err(|e| anyhow!(e))?);

        let transaction = build_sell_transaction_with_tip(&tip_account, &payer, priority_fee, instructions.clone(), recent_blockhash)?;
        transactions.push(transaction);
    }

    let mut handles = vec![];
    for i in 0..fee_clients.len() {
        let fee_client = fee_clients[i].clone();
        let transaction = transactions[i].clone();
        let handle: JoinHandle<Result<(), anyhow::Error>> = tokio::spawn(async move {
            fee_client.send_transaction(&transaction).await?;
            println!("index: {}, Total Jito sell operation time: {:?}ms", i, start_time.elapsed().as_millis());
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

    println!("Total Jito sell operation time: {:?}ms", start_time.elapsed().as_millis());
    Ok(())
}

pub fn sell_with_tip_ex(
    fee_clients: Vec<Arc<FeeClient>>,
    payer: &Keypair,
    mint: &Pubkey,
    amount_token: u64,
    amount_sol: u64,
    close_mint_ata: bool,
    slippage_basis_points: u64,
    priority_fee: PriorityFee,
    recent_blockhash: Hash,
) -> Result<Vec<String>, anyhow::Error> {
    let mut transactions = vec![];
    let instructions = build_sell_instructions_ex(&payer, &mint, amount_token, amount_sol, close_mint_ata, Some(slippage_basis_points))?;

    for fee_client in fee_clients.clone() {
        let priority_fee = priority_fee.clone();
        let tip_account = fee_client.get_tip_account().map_err(|e| anyhow!(e.to_string()))?;
        let tip_account = Pubkey::from_str(&tip_account).map_err(|e| anyhow!(e))?;

        let transaction = build_sell_transaction_with_tip(&tip_account, payer, priority_fee, instructions.clone(), recent_blockhash)?;
        transactions.push(transaction);
    }

    let mut tx_hashs = vec![];
    for i in 0..fee_clients.len() {
        let fee_client = fee_clients[i].clone();
        let transaction = transactions[i].clone();
        let tx_hash = transaction.signatures[0].to_string();
        log::info!("send sell tx: {} {:?} {}", mint, fee_client.get_client_type(), tx_hash);
        tx_hashs.push(tx_hash);
        tokio::spawn(async move {
            fee_client.send_transaction(&transaction).await
        });
    }

    Ok(tx_hashs)
}

pub async fn build_sell_transaction(
    rpc: Arc<SolanaRpcClient>,
    payer: &Keypair,
    priority_fee: PriorityFee,
    build_instructions: Vec<Instruction>,
) -> Result<Transaction, anyhow::Error> {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(priority_fee.unit_limit),
    ];

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

pub fn build_sell_transaction_sync(
    payer: &Keypair,
    priority_fee: PriorityFee,
    build_instructions: Vec<Instruction>,
    recent_blockhash: Hash
) -> Transaction {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(priority_fee.unit_limit),
    ];

    instructions.extend(build_instructions);

    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    transaction
}

pub fn build_sell_transaction_with_tip(
    tip_account: &Pubkey,
    payer: &Keypair,
    priority_fee: PriorityFee,
    build_instructions: Vec<Instruction>,
    blockhash: Hash,
) -> Result<VersionedTransaction, anyhow::Error> {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee.unit_price),
        ComputeBudgetInstruction::set_compute_unit_limit(priority_fee.unit_limit),
    ];

    instructions.extend(build_instructions);
    instructions.push(system_instruction::transfer(&payer.pubkey(), &tip_account, sol_to_lamports(priority_fee.sell_tip_fee)));

    let v0_message: v0::Message =
        v0::Message::try_compile(&payer.pubkey(), &instructions, &[], blockhash)?;
    let versioned_message: VersionedMessage = VersionedMessage::V0(v0_message);

    let transaction = VersionedTransaction::try_new(versioned_message, &[&payer])?;

    Ok(transaction)
}

pub async fn build_sell_instructions(
    rpc: Arc<SolanaRpcClient>,
    payer: Arc<Keypair>,
    mint: Pubkey,
    amount_token: Option<u64>,
    slippage_basis_points: Option<u64>,
) -> Result<Vec<Instruction>, anyhow::Error> {
    let (balance_u64, ata) = get_token_balance(rpc.as_ref(), payer.as_ref(), &mint).await?;
    let amount = amount_token.unwrap_or(balance_u64);
    
    if amount == 0 {
        return Err(anyhow!("Amount cannot be zero"));
    }
    
    let global_account = get_global_account(rpc.as_ref()).await?;
    let bonding_curve_account = get_bonding_curve_account(rpc.as_ref(), &mint).await?;
    let min_sol_output = bonding_curve_account
        .get_sell_price(amount, global_account.fee_basis_points)
        .map_err(|e| anyhow!(e))?;
    let min_sol_output_with_slippage = calculate_with_slippage_sell(
        min_sol_output,
        slippage_basis_points.unwrap_or(DEFAULT_SLIPPAGE),
    );

    let instructions = vec![
        instruction::sell(
            payer.as_ref(),
            &mint,
            &global_account.fee_recipient,
            instruction::Sell {
                _amount: amount,
                _min_sol_output: min_sol_output_with_slippage,
            },
        ),
    
        close_account(
            &spl_token::ID,
            &ata,
            &payer.pubkey(),
            &payer.pubkey(),
            &[&payer.pubkey()],
        )?
    ];

    Ok(instructions)
}

pub fn build_sell_instructions_ex(
    payer: &Keypair,
    mint: &Pubkey,
    amount_token: u64,
    amount_sol: u64,
    close_mint_ata: bool,
    slippage_basis_points: Option<u64>,
) -> Result<Vec<Instruction>, anyhow::Error> {
    let global_account = get_global_account_cache();
    let min_sol_output = calculate_with_slippage_sell(amount_sol,slippage_basis_points.unwrap_or(DEFAULT_SLIPPAGE));

    let mut instructions = vec![
        instruction::sell(
            payer,
            &mint,
            &global_account.fee_recipient,
            instruction::Sell {
                _amount: amount_token,
                _min_sol_output: min_sol_output,
            },
        ),
    ];
    
    if close_mint_ata {
        let mint_ata = get_associated_token_address(&payer.pubkey(), &mint);
        instructions.push(close_account(
            &spl_token::ID,
            &mint_ata,
            &payer.pubkey(),
            &payer.pubkey(),
            &[&payer.pubkey()],
        )?);
    }

    Ok(instructions)
}
