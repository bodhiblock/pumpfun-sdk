use crate::{
    accounts::{self, AMMPoolAccount},
    common::{logs_data::TradeInfo, PriorityFee, SolanaRpcClient},
    constants::{
        self,
        accounts::{PUMPFUN_AMM, PUMPFUN_AMM_GLOBAL, WSOL},
        trade::DEFAULT_SLIPPAGE,
    },
};
use anyhow::anyhow;
use borsh::BorshDeserialize;
use once_cell::sync::OnceCell;
use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction, instruction::Instruction, pubkey::Pubkey,
    signature::Keypair, signer::Signer, system_instruction, transaction::Transaction,
};
use spl_associated_token_account::get_associated_token_address;
use std::sync::RwLock;
use std::{collections::HashMap, sync::Arc};

lazy_static::lazy_static! {
    static ref ACCOUNT_CACHE: RwLock<HashMap<Pubkey, Arc<accounts::GlobalAccount>>> = RwLock::new(HashMap::new());
    static ref AMM_GLOBAL_ACCOUNT: OnceCell<Arc<accounts::AMMGlobalAccount>> = OnceCell::new();
}

pub async fn transfer_sol(
    rpc: &SolanaRpcClient,
    payer: &Keypair,
    receive_wallet: &Pubkey,
    amount: u64,
) -> Result<(), anyhow::Error> {
    if amount == 0 {
        return Err(anyhow!("transfer_sol: Amount cannot be zero"));
    }

    let balance = get_sol_balance(rpc, &payer.pubkey()).await?;
    if balance < amount {
        return Err(anyhow!("Insufficient balance"));
    }

    let transfer_instruction =
        system_instruction::transfer(&payer.pubkey(), receive_wallet, amount);

    let recent_blockhash = rpc.get_latest_blockhash().await?;

    let transaction = Transaction::new_signed_with_payer(
        &[transfer_instruction],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    rpc.send_and_confirm_transaction(&transaction).await?;

    Ok(())
}

#[inline]
pub fn create_priority_fee_instructions(priority_fee: PriorityFee) -> Vec<Instruction> {
    let mut instructions = Vec::with_capacity(2);
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
        priority_fee.unit_limit,
    ));
    instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
        priority_fee.unit_price,
    ));

    instructions
}

// #[inline]
pub async fn get_token_balance(
    rpc: &SolanaRpcClient,
    payer: &Pubkey,
    mint: &Pubkey,
) -> Result<u64, anyhow::Error> {
    let ata = get_associated_token_address(payer, mint);
    // let account_data = rpc.get_account_data(&ata).await?;
    // let token_account = Account::unpack(&account_data.as_slice())?;

    // Ok(token_account.amount)

    // println!("get_token_balance ata: {}", ata);
    let balance = rpc.get_token_account_balance(&ata).await?;
    let balance_u64 = balance
        .amount
        .parse::<u64>()
        .map_err(|_| anyhow!("Failed to parse token balance"))?;
    Ok(balance_u64)
}

#[inline]
pub async fn get_token_balance_and_ata(
    rpc: &SolanaRpcClient,
    payer: &Keypair,
    mint: &Pubkey,
) -> Result<(u64, Pubkey), anyhow::Error> {
    let ata = get_associated_token_address(&payer.pubkey(), mint);
    // let account_data = rpc.get_account_data(&ata).await?;
    // let token_account = Account::unpack(&account_data)?;

    // Ok((token_account.amount, ata))

    let balance = rpc.get_token_account_balance(&ata).await?;
    let balance_u64 = balance
        .amount
        .parse::<u64>()
        .map_err(|_| anyhow!("Failed to parse token balance"))?;

    if balance_u64 == 0 {
        return Err(anyhow!("Balance is 0"));
    }

    Ok((balance_u64, ata))
}

#[inline]
pub async fn get_sol_balance(
    rpc: &SolanaRpcClient,
    account: &Pubkey,
) -> Result<u64, anyhow::Error> {
    println!("get_sol_balance account: {}", account);
    let balance = rpc.get_balance(account).await?;
    println!("get_sol_balance balance: {}", balance);
    Ok(balance)
}

#[inline]
pub fn get_global_pda() -> Pubkey {
    static GLOBAL_PDA: once_cell::sync::Lazy<Pubkey> = once_cell::sync::Lazy::new(|| {
        Pubkey::find_program_address(
            &[constants::seeds::GLOBAL_SEED],
            &constants::accounts::PUMPFUN,
        )
        .0
    });
    *GLOBAL_PDA
}

#[inline]
pub fn get_mint_authority_pda() -> Pubkey {
    static MINT_AUTHORITY_PDA: once_cell::sync::Lazy<Pubkey> = once_cell::sync::Lazy::new(|| {
        Pubkey::find_program_address(
            &[constants::seeds::MINT_AUTHORITY_SEED],
            &constants::accounts::PUMPFUN,
        )
        .0
    });
    *MINT_AUTHORITY_PDA
}

#[inline]
pub fn get_bonding_curve_pda(mint: &Pubkey) -> Option<Pubkey> {
    let seeds: &[&[u8]; 2] = &[constants::seeds::BONDING_CURVE_SEED, mint.as_ref()];
    let program_id: &Pubkey = &constants::accounts::PUMPFUN;
    let pda: Option<(Pubkey, u8)> = Pubkey::try_find_program_address(seeds, program_id);
    pda.map(|pubkey| pubkey.0)
}

#[inline]
pub fn get_metadata_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            constants::seeds::METADATA_SEED,
            constants::accounts::MPL_TOKEN_METADATA.as_ref(),
            mint.as_ref(),
        ],
        &constants::accounts::MPL_TOKEN_METADATA,
    )
    .0
}

#[inline]
pub async fn get_global_account(
    rpc: &SolanaRpcClient,
) -> Result<Arc<accounts::GlobalAccount>, anyhow::Error> {
    let global = get_global_pda();
    if let Some(account) = ACCOUNT_CACHE.read().unwrap().get(&global) {
        return Ok(account.clone());
    }

    let account = rpc.get_account(&global).await?;
    let global_account = bincode::deserialize::<accounts::GlobalAccount>(&account.data)?;
    let global_account = Arc::new(global_account);

    ACCOUNT_CACHE
        .write()
        .unwrap()
        .insert(global, global_account.clone());
    Ok(global_account)
}

#[inline]
pub async fn get_amm_global_account(
    rpc: &SolanaRpcClient,
) -> Result<Arc<accounts::AMMGlobalAccount>, anyhow::Error> {
    if let Some(account) = AMM_GLOBAL_ACCOUNT.get() {
        return Ok(account.clone());
    }

    let account = rpc.get_account(&PUMPFUN_AMM_GLOBAL).await?;
    let global_account = bincode::deserialize::<accounts::AMMGlobalAccount>(&account.data)?;
    let global_account = Arc::new(global_account);

    AMM_GLOBAL_ACCOUNT.set(global_account.clone()).unwrap();
    Ok(global_account)
}

#[inline]
pub fn get_global_account_cache() -> Arc<accounts::GlobalAccount> {
    let global = get_global_pda();
    ACCOUNT_CACHE.read().unwrap().get(&global).unwrap().clone()
}

#[inline]
pub fn get_amm_global_account_cache() -> Arc<accounts::AMMGlobalAccount> {
    AMM_GLOBAL_ACCOUNT.get().unwrap().clone()
}

#[inline]
pub async fn get_initial_buy_price(
    global_account: &Arc<accounts::GlobalAccount>,
    amount_sol: u64,
) -> Result<u64, anyhow::Error> {
    let buy_amount = global_account.get_initial_buy_price(amount_sol);
    Ok(buy_amount)
}

#[inline]
pub async fn get_bonding_curve_account(
    rpc: &SolanaRpcClient,
    mint: &Pubkey,
) -> Result<Arc<accounts::BondingCurveAccount>, anyhow::Error> {
    let bonding_curve_pda =
        get_bonding_curve_pda(mint).ok_or(anyhow!("Bonding curve not found"))?;

    let account = rpc.get_account(&bonding_curve_pda).await?;
    if account.data.is_empty() {
        return Err(anyhow!("Bonding curve not found"));
    }

    let bonding_curve = Arc::new(accounts::BondingCurveAccount::try_from_slice(
        &account.data,
    )?);
    Ok(bonding_curve)
}

#[inline]
pub async fn get_amm_pool_account(
    rpc: &SolanaRpcClient,
    pool: &Pubkey,
) -> Result<Arc<accounts::AMMPoolAccount>, anyhow::Error> {
    let account = rpc.get_account(pool).await?;
    if account.data.is_empty() {
        return Err(anyhow!("Pool not found"));
    }

    let pool_account = Arc::new(accounts::AMMPoolAccount::try_from_slice(&account.data)?);
    Ok(pool_account)
}

#[inline]
pub fn get_buy_amount_with_slippage(amount_sol: u64, slippage_basis_points: Option<u64>) -> u64 {
    let slippage = slippage_basis_points.unwrap_or(DEFAULT_SLIPPAGE);
    amount_sol + (amount_sol * slippage / 10000)
}

#[inline]
pub fn get_token_price(virtual_sol_reserves: u64, virtual_token_reserves: u64) -> f64 {
    let v_sol = virtual_sol_reserves as f64 / 100_000_000.0;
    let v_tokens = virtual_token_reserves as f64 / 100_000.0;
    v_sol / v_tokens
}

#[inline]
pub fn get_buy_price(amount: u64, trade_info: &TradeInfo) -> u64 {
    if amount == 0 {
        return 0;
    }

    let n: u128 =
        (trade_info.virtual_sol_reserves as u128) * (trade_info.virtual_token_reserves as u128);
    let i: u128 = (trade_info.virtual_sol_reserves as u128) + (amount as u128);
    let r: u128 = n / i + 1;
    let s: u128 = (trade_info.virtual_token_reserves as u128) - r;
    let s_u64 = s as u64;

    s_u64.min(trade_info.real_token_reserves)
}

#[inline]
pub fn calculate_with_slippage_buy(amount: u64, basis_points: u64) -> u64 {
    amount + (amount * basis_points) / 10000
}

#[inline]
pub fn calculate_with_slippage_sell(amount: u64, basis_points: u64) -> u64 {
    amount - (amount * basis_points) / 10000
}

pub fn amm_sell_get_sol_out(sol_reserve: u64, token_reserve: u64, token_in: u64) -> u64 {
    if token_in == 0 || sol_reserve == 0 || token_reserve == 0 {
        return 0; // 防止不合理输入
    }

    let sol_reserve = sol_reserve as u128;
    let token_reserve = token_reserve as u128;
    let token_in = token_in as u128;
    let numerator = sol_reserve.checked_mul(token_in).unwrap();
    let denominator = token_reserve.checked_add(token_in).unwrap();
    let sol_out = numerator.checked_div(denominator).unwrap();

    sol_out as u64
}

pub fn amm_buy_get_sol_in(sol_reserve: u64, token_reserve: u64, token_out: u64) -> u64 {
    if token_out == 0 || sol_reserve == 0 || token_reserve == 0 || token_out >= token_reserve {
        return 0; 
    }

    let sol_reserve = sol_reserve as u128;
    let token_reserve = token_reserve as u128;
    let token_in = token_out as u128;
    let numerator = sol_reserve.checked_mul(token_in).unwrap();
    let denominator = token_reserve.checked_sub(token_in).unwrap();
    let sol_out = numerator.checked_div(denominator).unwrap();

    sol_out as u64
}

pub fn amm_buy_get_token_out(sol_reserve: u64, token_reserve: u64, sol_in: u64) -> u64 {
    if sol_in == 0 || sol_reserve == 0 || token_reserve == 0 {
        return 0;
    }

    let invariant = sol_reserve as u128 * token_reserve as u128;
    let new_sol_reserve = sol_reserve as u128 + sol_in as u128;

    let new_token_reserve = invariant / new_sol_reserve;
    let token_out = token_reserve as u128 - new_token_reserve;

    token_out as u64
}

pub async fn get_amm_pool(
    rpc: &SolanaRpcClient,
    mint_address: &Pubkey,
) -> Result<(Pubkey, accounts::AMMPoolAccount), anyhow::Error> {
    let filters = vec![
        RpcFilterType::DataSize(211),
        RpcFilterType::Memcmp(Memcmp::new(
            43,
            MemcmpEncodedBytes::Base58(mint_address.to_string()),
        )),
        RpcFilterType::Memcmp(Memcmp::new(
            75,
            MemcmpEncodedBytes::Base58(WSOL.to_string()),
        )),
    ];

    let accounts = rpc
        .get_program_accounts_with_config(
            &PUMPFUN_AMM,
            RpcProgramAccountsConfig {
                filters: Some(filters),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    ..Default::default()
                },
                ..RpcProgramAccountsConfig::default()
            },
        )
        .await?;

    if accounts.is_empty() {
        return Err(anyhow!("No AMM pools found"));
    }

    if accounts.len() > 1 {
        return Err(anyhow!("Too many AMM pools found"));
    }

    let (pubkey, account) = &accounts[0];
    let pool_data = AMMPoolAccount::try_from_slice(&account.data)?;

    Ok((pubkey.clone(), pool_data))
}
